//! What the console's screens are made of, shared by `livestage-setup` and
//! `livestage-installer`: the colours, text fields, button rows and the small
//! dialogs. Drawn for the Linux text console: 80x25 at the least, its sixteen
//! colours, and no glyphs beyond the box-drawing set its font has.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

pub const ACCENT: Color = Color::Cyan;
pub const FOCUS: Color = Color::Yellow;
pub const GOOD: Color = Color::Green;
pub const BAD: Color = Color::Red;
pub const DIM: Color = Color::DarkGray;
pub const LABEL_WIDTH: u16 = 14;

pub fn cable_span(carrier: Option<bool>) -> Span<'static> {
    match carrier {
        Some(true) => Span::styled("cable in ", Style::new().fg(GOOD)),
        Some(false) => Span::styled("no cable ", Style::new().fg(FOCUS)),
        None => Span::styled("down     ", Style::new().fg(DIM)),
    }
}

pub fn button_row(labels: &[&str], focus: Option<usize>) -> Paragraph<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (index, label) in labels.iter().enumerate() {
        let style = if focus == Some(index) {
            Style::new().fg(Color::Black).bg(FOCUS).bold()
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!("[ {label} ]"), style));
        spans.push(Span::raw("  "));
    }
    Paragraph::new(Line::from(spans))
}

/// [`button_row`] for a narrow screen: `[Label]`, one space apart.
pub fn compact_button_row(labels: &[&str], focus: Option<usize>) -> Paragraph<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (index, label) in labels.iter().enumerate() {
        let style = if focus == Some(index) {
            Style::new().fg(Color::Black).bg(FOCUS).bold()
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!("[{label}]"), style));
        spans.push(Span::raw(" "));
    }
    Paragraph::new(Line::from(spans))
}

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

// ── Text fields ─────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
pub struct TextInput {
    pub value: String,
    /// In characters.
    pub cursor: usize,
    pub secret: bool,
}

impl TextInput {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            cursor: value.chars().count(),
            value,
            secret: false,
        }
    }

    pub fn secret() -> Self {
        Self {
            secret: true,
            ..Self::default()
        }
    }

    pub fn byte(&self, chars: usize) -> usize {
        self.value
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    /// True when the key was for the field.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let length = self.value.chars().count();
        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if length < 64 && !c.is_control() {
                    let at = self.byte(self.cursor);
                    self.value.insert(at, c);
                    self.cursor += 1;
                }
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte(self.cursor);
                self.value.remove(at);
            }
            KeyCode::Delete if self.cursor < length => {
                let at = self.byte(self.cursor);
                self.value.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(length),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = length,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    pub fn shown(&self) -> String {
        if self.secret {
            "*".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        }
    }
}

/// Lines a paragraph takes when wrapped to `width` (by words, roughly as
/// ratatui wraps).
pub fn wrapped_height(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    text.split('\n')
        .map(|paragraph| {
            let mut lines = 1;
            let mut used = 0;
            for word in paragraph.split_whitespace() {
                let length = word.chars().count();
                if used == 0 {
                    used = length;
                } else if used + 1 + length <= width {
                    used += 1 + length;
                } else {
                    lines += 1;
                    used = length;
                }
            }
            lines
        })
        .sum::<usize>() as u16
}

/// A question with No and Yes, Yes lit when `yes`.
pub fn draw_confirm(frame: &mut Frame, area: Rect, title: &str, text: &str, yes: bool) {
    let rect = centered(area, 60, 8);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::new().fg(FOCUS));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let [text_area, buttons] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
        .areas(inner.inner(Margin::new(1, 0)));
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), text_area);
    frame.render_widget(button_row(&["No", "Yes"], Some(usize::from(yes))), buttons);
}

/// A message that any key closes.
pub fn draw_message(frame: &mut Frame, area: Rect, title: &str, text: &str) {
    let rect = centered(area, 60, 7);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: true }).block(
            Block::bordered()
                .title(format!(" {title} "))
                .border_style(Style::new().fg(FOCUS))
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        rect,
    );
}
