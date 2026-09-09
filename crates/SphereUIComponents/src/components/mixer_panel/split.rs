use gpui::{App, Empty, IntoElement, Render, Window};

// ── Section dimensions ─────────────────────────────────────────────────────
//
// The per-section heights themselves live in [`super::console`], which owns the
// look; what lives here is what the splitter arithmetic needs — the strip's
// overall bounds and the fixed height it must leave alone.
pub const STRIP_WIDTH: f32 = 88.0;
/// Minimum height for a channel strip: everything on it that cannot give way.
/// Below this the mixer scrolls rather than compressing the pan/fader controls
/// into unusability.
///
/// Summed from the parts rather than written down. It used to be a flat 320,
/// which is 8 px less than a strip can actually be: the two racks bottom out at
/// [`SECTION_VIEWPORT_MIN_H`] each and every other section is `flex_none`, so
/// nothing could absorb the difference. The strip overflowed its own box, and
/// `overflow_hidden` paid for it by cutting the bottom off the name plate —
/// the strip's one piece of track colour, and the only thing on it that says
/// which channel you are looking at. Scrolling could not bring it back either,
/// because the scroll height was this same number.
///
/// A minimum smaller than the parts is not a minimum; it is a promise the
/// layout cannot keep. Derived, it cannot drift out of step with the sections
/// again — moving any of them moves this.
pub const STRIP_MIN_HEIGHT: f32 = STRIP_FIXED_H + (SECTION_VIEWPORT_MIN_H * 2.0);

/// Everything below the racks: I/O, pan, the fader bay at its floor, and the
/// toggle row. Fixed on every strip, which is what keeps the four kinds of
/// strip on one set of baselines.
pub(crate) const LOWER_CONTROL_MIN_H: f32 = super::console::IO_ROW_H
    + super::console::PAN_H
    + super::console::FADER_MIN_H
    + super::console::BUTTONS_H;

/// Everything on a strip that is not the two racks: the type row, both splitter
/// handles, the lower console at its floor, and the name plate. The racks are
/// the only sections that resize, so this is the height they may never eat
/// into — and, with both racks at their own floor, the whole of what a strip
/// cannot be shorter than (see [`STRIP_MIN_HEIGHT`]).
pub(crate) const STRIP_FIXED_H: f32 = super::console::TOP_ROW_H
    + (SEC_SPLITTER_H * 2.0)
    + LOWER_CONTROL_MIN_H
    + super::console::PLATE_H;

// ── Vertical mixer section resizing ─────────────────────────────────────────
// Inserts and sends each own a fixed-height clipped viewport with their own
// vertical scrolling. Heights are shared across all strips so rows stay aligned
// across the mixer. Splitter actions are routed to `StudioLayout`, which owns
// the shared values and mirrors them into the detached mixer window snapshot.
/// Visual + hitbox height of the splitter handle.
pub(crate) const SEC_SPLITTER_H: f32 = 6.0;
const SECTION_VIEWPORT_MIN_H: f32 = 42.0;
const SECTION_VIEWPORT_MAX_H: f32 = 180.0;
/// Default height of the inserts viewport.
///
/// A fresh session has no inserts, so this reserves a visible gap between
/// INSERTS and SENDS — but it cannot simply be lowered. The Monitor strip's
/// Source selector deliberately occupies this same slot to keep the two pinned
/// strips on matching baselines (`monitor_strip`), and shortening it collapses
/// that selector to an ellipsis. Tightening the empty bay needs the Monitor's
/// routing block decoupled from the inserts height first.
pub const MIXER_INSERT_SECTION_DEFAULT_PX: f32 = 72.0;
pub const MIXER_SEND_SECTION_DEFAULT_PX: f32 = 54.0;

/// Clamp one insert/send section height into the static supported range.
pub fn clamp_mixer_section_height_px(value: f32) -> f32 {
    value.clamp(SECTION_VIEWPORT_MIN_H, SECTION_VIEWPORT_MAX_H)
}

/// Clamp both section heights while preserving a usable lower pan/fader area
/// for the current strip allocation.
pub fn clamp_mixer_section_heights_for_strip(
    insert_px: f32,
    send_px: f32,
    strip_available_px: f32,
) -> (f32, f32) {
    let mut insert_px = clamp_mixer_section_height_px(insert_px);
    let mut send_px = clamp_mixer_section_height_px(send_px);
    // The racks get whatever the fixed sections leave, and never less than their
    // own floor — which is exactly why `STRIP_MIN_HEIGHT` has to be the sum of
    // the two: at that height this floor is the whole remainder, and a strip
    // laid out any shorter overflows however hard the racks shrink.
    let max_total = (strip_available_px - STRIP_FIXED_H).max(SECTION_VIEWPORT_MIN_H * 2.0);

    let total = insert_px + send_px;
    if total > max_total {
        let overflow = total - max_total;
        let shrinkable_insert = insert_px - SECTION_VIEWPORT_MIN_H;
        let shrinkable_send = send_px - SECTION_VIEWPORT_MIN_H;
        let shrinkable_total = shrinkable_insert + shrinkable_send;
        if shrinkable_total > 0.0 {
            insert_px -= overflow * (shrinkable_insert / shrinkable_total);
            send_px -= overflow * (shrinkable_send / shrinkable_total);
        }
        insert_px = clamp_mixer_section_height_px(insert_px);
        send_px = clamp_mixer_section_height_px(send_px);
    }

    (insert_px, send_px)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MixerSplitTarget {
    InsertSend,
    SendFader,
}

/// Splitter drag/reset intents emitted by the channel-strip splitter handle.
/// Pointer Y values are window-space (matches `MouseDownEvent::position.y`).
#[derive(Clone, Copy, Debug)]
pub enum MixerSplitAction {
    /// Pointer pressed on the splitter — record the drag anchor.
    ResizeStart(MixerSplitTarget, f32),
    /// Pointer moved while dragging — recompute the shared rack height.
    ResizeMove(f32),
    /// Pointer released — commit the drag.
    ResizeEnd,
    /// Double-click — reset the targeted section to its default height.
    Reset(MixerSplitTarget),
}

/// Shared split layout passed into the mixer. Insert/send heights are already
/// clamped by the owner; `on_action` routes splitter intents back to the owner
/// so all strips resize together.
#[derive(Clone)]
pub struct MixerSplit {
    pub insert_px: f32,
    pub send_px: f32,
    pub active_target: Option<MixerSplitTarget>,
    pub on_action: std::sync::Arc<dyn Fn(MixerSplitAction, &mut Window, &mut App) + 'static>,
}

impl MixerSplit {
    /// Inert split for fallback UI (no live owner to route drags to).
    pub fn inert() -> Self {
        Self {
            insert_px: MIXER_INSERT_SECTION_DEFAULT_PX,
            send_px: MIXER_SEND_SECTION_DEFAULT_PX,
            active_target: None,
            on_action: std::sync::Arc::new(|_, _, _| {}),
        }
    }
}

/// Zero-sized GPUI drag payload for the mixer splitter handle. Mirrors the
/// bottom-panel resize pattern: `on_drag` registers it, `on_drag_move` on the
/// mixer root recomputes height while the pointer is captured.
#[derive(Clone, Copy, Debug, Default)]
pub struct MixerSplitDrag;

impl Render for MixerSplitDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[cfg(test)]
mod tests {
    // `console` is a sibling of `split`, so from inside this module it is
    // two levels up rather than one.
    use super::super::console;
    use super::*;

    /// The height a strip actually lays out at, given a pair of rack heights.
    fn laid_out_height(insert_px: f32, send_px: f32) -> f32 {
        console::TOP_ROW_H
            + insert_px
            + SEC_SPLITTER_H
            + send_px
            + SEC_SPLITTER_H
            + LOWER_CONTROL_MIN_H
            + console::PLATE_H
    }

    #[test]
    fn the_strip_minimum_is_the_sum_of_the_sections_it_is_made_of() {
        // Spelled out independently of the constant, so a section that moves
        // without the minimum moving is caught here rather than on screen.
        let sections = console::TOP_ROW_H
            + SECTION_VIEWPORT_MIN_H
            + SEC_SPLITTER_H
            + SECTION_VIEWPORT_MIN_H
            + SEC_SPLITTER_H
            + LOWER_CONTROL_MIN_H
            + console::PLATE_H;
        assert_eq!(
            STRIP_MIN_HEIGHT, sections,
            "a strip cannot be laid out shorter than the sections it contains"
        );
    }

    #[test]
    fn a_strip_at_its_minimum_fits_its_name_plate() {
        // The regression: at the old flat 320 the racks bottomed out at 42 each
        // and the strip still needed 328, so `overflow_hidden` cut 8 px off the
        // 24 px name plate — the track colour and the channel name with it.
        let (insert, send) = clamp_mixer_section_heights_for_strip(
            MIXER_INSERT_SECTION_DEFAULT_PX,
            MIXER_SEND_SECTION_DEFAULT_PX,
            STRIP_MIN_HEIGHT,
        );
        assert!(
            laid_out_height(insert, send) <= STRIP_MIN_HEIGHT,
            "strip needs {} px inside a {} px box",
            laid_out_height(insert, send),
            STRIP_MIN_HEIGHT
        );
    }

    #[test]
    fn no_strip_height_ever_overflows_its_own_box() {
        // Every height the panel can hand a strip, at every rack size a saved
        // session can carry. The clamp is the only thing standing between a
        // stored split and a clipped plate.
        let mut height = STRIP_MIN_HEIGHT;
        while height <= 900.0 {
            for &insert in &[
                SECTION_VIEWPORT_MIN_H,
                MIXER_INSERT_SECTION_DEFAULT_PX,
                SECTION_VIEWPORT_MAX_H,
                1_000.0,
            ] {
                for &send in &[
                    SECTION_VIEWPORT_MIN_H,
                    MIXER_SEND_SECTION_DEFAULT_PX,
                    SECTION_VIEWPORT_MAX_H,
                    1_000.0,
                ] {
                    let (i, s) = clamp_mixer_section_heights_for_strip(insert, send, height);
                    assert!(
                        laid_out_height(i, s) <= height + 0.001,
                        "height {height}: {insert}/{send} clamped to {i}/{s} still needs {}",
                        laid_out_height(i, s)
                    );
                }
            }
            height += 7.0;
        }
    }

    #[test]
    fn the_racks_keep_their_defaults_when_there_is_room_for_them() {
        // The clamp exists to protect the plate, not to shrink racks that fit.
        let roomy = STRIP_MIN_HEIGHT + 200.0;
        let (insert, send) = clamp_mixer_section_heights_for_strip(
            MIXER_INSERT_SECTION_DEFAULT_PX,
            MIXER_SEND_SECTION_DEFAULT_PX,
            roomy,
        );
        assert_eq!(insert, MIXER_INSERT_SECTION_DEFAULT_PX);
        assert_eq!(send, MIXER_SEND_SECTION_DEFAULT_PX);
    }
}
