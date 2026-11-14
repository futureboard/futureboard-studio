//! Bounded memoization for the platform-shaped width used before truncation.

use crate::{FontId, Pixels};
use collections::FxHashMap;
use parking_lot::RwLock;
use std::{collections::VecDeque, sync::Arc};

const MAX_ENTRIES: usize = 4096;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 4096;

type FontKey = (FontId, u32);

#[derive(Default)]
pub(super) struct StringWidthCache(RwLock<Widths>);

#[derive(Default)]
struct Widths {
    by_font: FxHashMap<FontKey, FxHashMap<Arc<str>, Pixels>>,
    insertion_order: VecDeque<(FontKey, Arc<str>)>,
    text_bytes: usize,
    generation: u64,
}

impl StringWidthCache {
    pub(super) fn get_or_measure(
        &self,
        font_id: FontId,
        font_size: Pixels,
        text: &str,
        measure: impl FnOnce() -> Pixels,
    ) -> Pixels {
        // Large editor buffers are not UI labels. Do not retain or copy them.
        if text.len() > MAX_ENTRY_BYTES {
            return measure();
        }
        let font_key = (font_id, font_size.0.to_bits());
        let generation = {
            let widths = self.0.read();
            if let Some(width) = widths.by_font.get(&font_key).and_then(|map| map.get(text)) {
                return *width;
            }
            widths.generation
        };

        // Platform shaping can be slow. Never hold the cache lock while it
        // runs, and do not publish a result started before fonts were added.
        let width = measure();
        let mut widths = self.0.write();
        if widths.generation == generation {
            widths.insert(font_key, text, width);
        }
        width
    }

    pub(super) fn clear(&self) {
        let mut widths = self.0.write();
        let generation = widths.generation.wrapping_add(1);
        *widths = Widths {
            generation,
            ..Default::default()
        };
    }
}

impl Widths {
    fn insert(&mut self, font_key: FontKey, text: &str, width: Pixels) {
        if self
            .by_font
            .get(&font_key)
            .is_some_and(|map| map.contains_key(text))
        {
            return;
        }
        while self.insertion_order.len() >= MAX_ENTRIES
            || self.text_bytes + text.len() > MAX_TEXT_BYTES
        {
            let Some((font, text)) = self.insertion_order.pop_front() else {
                break;
            };
            if let Some(map) = self.by_font.get_mut(&font) {
                map.remove(text.as_ref());
                if map.is_empty() {
                    self.by_font.remove(&font);
                }
            }
            self.text_bytes -= text.len();
        }
        let text: Arc<str> = text.into();
        self.text_bytes += text.len();
        self.by_font
            .entry(font_key)
            .or_default()
            .insert(text.clone(), width);
        self.insertion_order.push_back((font_key, text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px;
    use std::cell::Cell;

    #[test]
    fn repeated_measurements_shape_once_per_text_font_and_size() {
        let cache = StringWidthCache::default();
        let calls = Cell::new(0);
        let measure = || {
            calls.set(calls.get() + 1);
            px(73.25)
        };
        for _ in 0..1000 {
            assert_eq!(
                cache.get_or_measure(FontId(1), px(12.0), "หลงอากาศ", measure),
                px(73.25)
            );
        }
        assert_eq!(calls.get(), 1);
        cache.get_or_measure(FontId(2), px(12.0), "หลงอากาศ", measure);
        cache.get_or_measure(FontId(1), px(14.0), "หลงอากาศ", measure);
        cache.get_or_measure(FontId(1), px(12.0), "Audio 1", measure);
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn clearing_fonts_does_not_publish_an_in_flight_old_measurement() {
        let cache = StringWidthCache::default();
        assert_eq!(
            cache.get_or_measure(FontId(1), px(12.0), "label", || {
                cache.clear();
                px(10.0)
            }),
            px(10.0)
        );
        assert_eq!(
            cache.get_or_measure(FontId(1), px(12.0), "label", || px(20.0)),
            px(20.0)
        );
        cache.clear();
        assert_eq!(
            cache.get_or_measure(FontId(1), px(12.0), "label", || px(30.0)),
            px(30.0)
        );
    }

    #[test]
    fn retention_is_bounded_by_entry_count_and_text_bytes() {
        let cache = StringWidthCache::default();
        for font in 0..MAX_ENTRIES + 1 {
            cache.get_or_measure(FontId(font), px(12.0), "label", || px(10.0));
        }
        {
            let widths = cache.0.read();
            assert_eq!(widths.insertion_order.len(), MAX_ENTRIES);
            assert_eq!(widths.by_font.len(), MAX_ENTRIES);
            assert!(!widths.by_font.contains_key(&(FontId(0), 12.0f32.to_bits())));
        }

        cache.clear();
        for i in 0..400 {
            let text = format!("{i:04}{}", "x".repeat(4000));
            cache.get_or_measure(FontId(1), px(12.0), &text, || px(10.0));
        }
        let widths = cache.0.read();
        assert!(widths.text_bytes <= MAX_TEXT_BYTES);
        assert_eq!(widths.text_bytes, widths.insertion_order.len() * 4004);
        assert!(widths.insertion_order.len() < 400);
    }

    #[test]
    fn oversized_text_bypasses_retention() {
        let cache = StringWidthCache::default();
        let text = "x".repeat(MAX_ENTRY_BYTES + 1);
        let calls = Cell::new(0);
        for _ in 0..2 {
            cache.get_or_measure(FontId(1), px(12.0), &text, || {
                calls.set(calls.get() + 1);
                px(10.0)
            });
        }
        assert_eq!(calls.get(), 2);
        assert!(cache.0.read().insertion_order.is_empty());
    }
}
