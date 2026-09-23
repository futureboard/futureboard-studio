//! Shared numeric edit-session core for typed + drag numeric fields.
//!
//! This is the value/commit *policy* layer that sits on top of the shared
//! [`crate::components::text_input::TextInputState`] for numeric fields such as
//! the transport BPM display, time-signature numerator/denominator, count-in
//! values, and inspector numeric entries.
//!
//! Design goals (see the shared TextInput rollout spec, Part 1):
//!
//! * **One value source.** The typed draft always lives in the field's
//!   `TextInputState.value`; this module never stores a second copy of the
//!   draft text. Drag/scrub and typed editing therefore resolve through the
//!   same draft/model value instead of two competing state machines.
//! * **Intermediate drafts stay editable.** Partial input such as `"-"`,
//!   `"."`, or `"1."` must not be rejected mid-edit; they simply are not
//!   *committable* yet.
//! * **Clamp at commit.** Range clamping happens when a value is committed (or
//!   previewed for a drag), not while the user is still typing.
//! * **No model mutation on invalid input.** [`NumericEditSession::commit`]
//!   returns `None` for non-committable drafts, so the caller performs exactly
//!   one model command on a valid commit and none otherwise — no project
//!   history entry per keystroke.
//!
//! The type is intentionally pure (no GPUI dependency) so the numeric lifecycle
//! is unit-testable without a window/app harness.

/// When a numeric field pushes its draft into the backing model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitPolicy {
    /// Every committable draft change writes to the model immediately (live).
    Immediate,
    /// Commit only on Enter / explicit submit.
    OnEnter,
    /// Commit when keyboard focus leaves the field.
    OnBlur,
    /// Commit only via an explicit Apply action.
    ExplicitApply,
}

/// The trigger that is asking whether a commit should happen now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitTrigger {
    /// The draft text changed.
    Change,
    /// The user pressed Enter / submitted.
    Enter,
    /// Focus left the field.
    Blur,
    /// An explicit Apply action fired.
    Apply,
}

/// Numeric formatting + range policy for a single field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NumberFormat {
    /// Inclusive lower bound applied at commit/preview time.
    pub min: f64,
    /// Inclusive upper bound applied at commit/preview time.
    pub max: f64,
    /// Number of fractional digits used when formatting a value to text.
    pub decimals: u8,
    /// Increment applied by one arrow-key nudge step.
    pub step: f64,
    /// Whether negative values are accepted.
    pub allow_negative: bool,
    /// Whether the field only accepts whole numbers (fractional drafts are not
    /// committable, and committed values are rounded to the nearest integer).
    pub require_integer: bool,
}

impl NumberFormat {
    /// A decimal field with the given inclusive range and fractional digits.
    pub fn decimal(min: f64, max: f64, decimals: u8) -> Self {
        Self {
            min,
            max,
            decimals,
            step: 1.0,
            allow_negative: min < 0.0,
            require_integer: false,
        }
    }

    /// An integer field with the given inclusive range.
    pub fn integer(min: f64, max: f64) -> Self {
        Self {
            min,
            max,
            decimals: 0,
            step: 1.0,
            allow_negative: min < 0.0,
            require_integer: true,
        }
    }

    /// Override the arrow-key nudge step.
    pub fn with_step(mut self, step: f64) -> Self {
        self.step = step.abs();
        self
    }

    /// Override whether negative values are accepted.
    pub fn with_allow_negative(mut self, allow: bool) -> Self {
        self.allow_negative = allow;
        self
    }

    /// Clamp a raw value into `[min, max]` (rounding to an integer first when
    /// the field requires integers).
    pub fn clamp(&self, value: f64) -> f64 {
        let value = if self.require_integer {
            value.round()
        } else {
            value
        };
        value.clamp(self.min, self.max)
    }

    /// Format a numeric value as display text, trimming redundant trailing
    /// zeros / decimal point so integral values render without a fraction.
    pub fn format(&self, value: f64) -> String {
        if self.require_integer || self.decimals == 0 {
            return format!("{:.0}", value.round());
        }
        let mut text = format!("{:.*}", self.decimals as usize, value);
        if text.contains('.') {
            while text.ends_with('0') {
                text.pop();
            }
            if text.ends_with('.') {
                text.pop();
            }
        }
        text
    }
}

/// Whether `draft` is a still-in-progress numeric entry that must remain
/// editable but is not yet committable (e.g. `""`, `"-"`, `"."`, `"1."`).
pub fn is_intermediate_draft(draft: &str, allow_negative: bool) -> bool {
    let trimmed = draft.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed == "-" || trimmed == "+" {
        return allow_negative || trimmed == "+";
    }
    // A lone or trailing decimal point (optionally signed) is still editable.
    matches!(trimmed, "." | "-." | "+.") || trimmed.ends_with('.')
}

/// The draft as Rust's float parser reads it: native digits (Thai ๐–๙ and
/// the other scripts [`ascii_digit_equivalent`] knows) become ASCII, and a
/// lone comma is a decimal point — `120,5` is how half the world writes
/// 120.5, and rejecting it made Enter look broken.
///
/// [`ascii_digit_equivalent`]: crate::components::text_input::ascii_digit_equivalent
fn normalize_draft(draft: &str) -> String {
    let mut out: String = draft
        .chars()
        .map(|c| crate::components::text_input::ascii_digit_equivalent(c).unwrap_or(c))
        .collect();
    if !out.contains('.') && out.matches(',').count() == 1 {
        out = out.replace(',', ".");
    }
    out
}

/// Parse a numeric draft, honouring the sign/integer policy. Returns `None`
/// for empty, intermediate, or malformed drafts.
pub fn parse_draft(draft: &str, format: &NumberFormat) -> Option<f64> {
    let normalized = normalize_draft(draft);
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: f64 = trimmed.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    if !format.allow_negative && value < 0.0 {
        return None;
    }
    if format.require_integer && value.fract().abs() > f64::EPSILON {
        return None;
    }
    Some(value)
}

/// A numeric editing session: owns the original value plus the field's
/// format/commit policy. The live draft text is owned by the field's
/// `TextInputState`, so this type stays [`Copy`] and free of borrowed state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NumericEditSession {
    original: f64,
    format: NumberFormat,
    policy: CommitPolicy,
}

impl NumericEditSession {
    /// Begin an edit session anchored to `original`.
    pub fn begin(original: f64, format: NumberFormat, policy: CommitPolicy) -> Self {
        Self {
            original: format.clamp(original),
            format,
            policy,
        }
    }

    /// The value captured at the start of the edit.
    pub fn original(&self) -> f64 {
        self.original
    }

    /// The original value rendered as the initial draft text.
    pub fn original_text(&self) -> String {
        self.format.format(self.original)
    }

    /// The field's numeric format/range policy.
    pub fn format(&self) -> &NumberFormat {
        &self.format
    }

    /// The field's commit policy.
    pub fn policy(&self) -> CommitPolicy {
        self.policy
    }

    /// Whether `draft` is editable-but-not-yet-committable.
    pub fn is_intermediate(&self, draft: &str) -> bool {
        is_intermediate_draft(draft, self.format.allow_negative)
    }

    /// Whether `draft` parses to a real, in-policy number.
    pub fn is_committable(&self, draft: &str) -> bool {
        parse_draft(draft, &self.format).is_some()
    }

    /// The clamped value a valid `draft` would resolve to, for live drag/preview
    /// use. Returns `None` for intermediate or malformed drafts.
    pub fn preview(&self, draft: &str) -> Option<f64> {
        parse_draft(draft, &self.format).map(|v| self.format.clamp(v))
    }

    /// The clamped value to commit for `draft`, or `None` when nothing should be
    /// written to the model (intermediate/invalid draft). Clamping happens here,
    /// never while typing.
    pub fn commit(&self, draft: &str) -> Option<f64> {
        self.preview(draft)
    }

    /// The value to restore when the edit is cancelled (Escape).
    pub fn cancel(&self) -> f64 {
        self.original
    }

    /// Whether a commit should fire for `trigger` under this session's policy.
    pub fn should_commit_on(&self, trigger: CommitTrigger) -> bool {
        match self.policy {
            CommitPolicy::Immediate => matches!(
                trigger,
                CommitTrigger::Change | CommitTrigger::Enter | CommitTrigger::Blur
            ),
            CommitPolicy::OnEnter => matches!(trigger, CommitTrigger::Enter),
            CommitPolicy::OnBlur => matches!(trigger, CommitTrigger::Enter | CommitTrigger::Blur),
            CommitPolicy::ExplicitApply => matches!(trigger, CommitTrigger::Apply),
        }
    }

    /// Produce a new draft text after an arrow-key nudge of `steps` (fractional
    /// multipliers allowed for fine/coarse). The current draft value is used as
    /// the base; intermediate drafts fall back to the original value. The result
    /// is clamped and re-formatted, so the field stays a single value source.
    pub fn nudge_draft(&self, draft: &str, steps: f64) -> String {
        let base = parse_draft(draft, &self.format).unwrap_or(self.original);
        let next = self.format.clamp(base + steps * self.format.step);
        self.format.format(next)
    }

    /// Resolve a drag adjustment: clamp `base + delta` into range. Typed edits
    /// and drag adjustments therefore share the same clamp/value resolution.
    pub fn drag_value(&self, base: f64, delta: f64) -> f64 {
        self.format.clamp(base + delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bpm_format() -> NumberFormat {
        NumberFormat::decimal(20.0, 999.0, 2)
    }

    fn ts_format() -> NumberFormat {
        NumberFormat::integer(1.0, 64.0)
    }

    #[test]
    fn typed_bpm_commit() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        assert_eq!(s.commit("140"), Some(140.0));
        assert_eq!(s.commit("128.5"), Some(128.5));
    }

    #[test]
    fn invalid_bpm_commit_is_rejected() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        assert_eq!(s.commit("abc"), None);
        assert_eq!(s.commit(""), None);
        assert_eq!(s.commit("12x"), None);
        // Invalid commit performs no model mutation, so the caller keeps the
        // original value untouched.
        assert_eq!(s.cancel(), 120.0);
    }

    #[test]
    fn escape_restores_original() {
        let s = NumericEditSession::begin(137.0, bpm_format(), CommitPolicy::OnEnter);
        assert_eq!(s.cancel(), 137.0);
    }

    #[test]
    fn intermediate_drafts_stay_editable() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        // Every partial entry stays editable (never reformatted/rejected mid-type).
        for draft in ["", ".", "1.", "12."] {
            assert!(s.is_intermediate(draft), "{draft:?} should be intermediate");
        }
        // Drafts without a parseable number yet are not committable.
        for draft in ["", "."] {
            assert!(!s.is_committable(draft), "{draft:?} not committable yet");
            assert_eq!(s.commit(draft), None, "{draft:?} must not commit");
        }
        // A trailing-dot draft with digits is editable *and* parseable, so it
        // may commit its numeric value (Enter on "12." commits 12).
        assert!(s.is_committable("12."));
        assert_eq!(s.commit("12."), Some(12.0).map(|v| bpm_format().clamp(v)));
        // Negative signs only count as intermediate when negatives are allowed.
        assert!(!s.is_intermediate("-"));
    }

    #[test]
    fn signed_intermediate_when_negative_allowed() {
        let fmt = NumberFormat::decimal(-100.0, 100.0, 2);
        let s = NumericEditSession::begin(0.0, fmt, CommitPolicy::OnEnter);
        assert!(s.is_intermediate("-"));
        assert!(s.is_intermediate("-."));
        assert!(!s.is_committable("-"));
        assert_eq!(s.commit("-12.5"), Some(-12.5));
    }

    #[test]
    fn clamps_only_at_commit() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        assert_eq!(s.commit("5"), Some(20.0)); // below min
        assert_eq!(s.commit("2000"), Some(999.0)); // above max
        assert_eq!(s.commit("120"), Some(120.0)); // in range
    }

    #[test]
    fn drag_then_typed_edit_share_value_source() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        // Drag from the current value.
        let dragged = s.drag_value(120.0, 15.0);
        assert_eq!(dragged, 135.0);
        // A new session anchored at the dragged value; a typed edit overrides.
        let s2 = NumericEditSession::begin(dragged, bpm_format(), CommitPolicy::OnEnter);
        assert_eq!(s2.original_text(), "135");
        assert_eq!(s2.commit("90"), Some(90.0));
    }

    #[test]
    fn typed_edit_then_drag_share_value_source() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        // Typed preview feeds the drag base.
        let base = s.preview("100").unwrap();
        assert_eq!(base, 100.0);
        assert_eq!(s.drag_value(base, -85.0), 20.0); // clamps at min during drag
    }

    #[test]
    fn drag_preview_clamps_into_range() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        assert_eq!(s.drag_value(990.0, 50.0), 999.0);
        assert_eq!(s.drag_value(25.0, -50.0), 20.0);
    }

    #[test]
    fn arrow_nudge_updates_draft() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        assert_eq!(s.nudge_draft("120", 1.0), "121");
        assert_eq!(s.nudge_draft("120", -1.0), "119");
        // Coarse step (Shift) via multiplier.
        assert_eq!(s.nudge_draft("120", 10.0), "130");
        // Intermediate draft nudges from the original value.
        assert_eq!(s.nudge_draft("", 1.0), "121");
        // Nudging clamps at the boundary.
        assert_eq!(s.nudge_draft("999", 5.0), "999");
    }

    #[test]
    fn time_signature_validation_rejects_non_integer_and_clamps() {
        let s = NumericEditSession::begin(4.0, ts_format(), CommitPolicy::OnEnter);
        assert_eq!(s.commit("3"), Some(3.0));
        assert_eq!(s.commit("3.5"), None); // fractional rejected for integer field
        assert_eq!(s.commit("0"), Some(1.0)); // clamps to min
        assert_eq!(s.commit("999"), Some(64.0)); // clamps to max
    }

    #[test]
    fn integer_format_renders_without_fraction() {
        let fmt = ts_format();
        assert_eq!(fmt.format(4.0), "4");
        let bpm = bpm_format();
        assert_eq!(bpm.format(120.0), "120");
        assert_eq!(bpm.format(128.5), "128.5");
        assert_eq!(bpm.format(128.50), "128.5");
    }

    #[test]
    fn commit_policies_gate_triggers() {
        let fmt = bpm_format();
        let immediate = NumericEditSession::begin(120.0, fmt, CommitPolicy::Immediate);
        assert!(immediate.should_commit_on(CommitTrigger::Change));
        assert!(immediate.should_commit_on(CommitTrigger::Blur));

        let on_enter = NumericEditSession::begin(120.0, fmt, CommitPolicy::OnEnter);
        assert!(on_enter.should_commit_on(CommitTrigger::Enter));
        assert!(!on_enter.should_commit_on(CommitTrigger::Change));
        assert!(!on_enter.should_commit_on(CommitTrigger::Blur));

        let on_blur = NumericEditSession::begin(120.0, fmt, CommitPolicy::OnBlur);
        assert!(on_blur.should_commit_on(CommitTrigger::Blur));
        assert!(on_blur.should_commit_on(CommitTrigger::Enter));
        assert!(!on_blur.should_commit_on(CommitTrigger::Change));

        let explicit = NumericEditSession::begin(120.0, fmt, CommitPolicy::ExplicitApply);
        assert!(explicit.should_commit_on(CommitTrigger::Apply));
        assert!(!explicit.should_commit_on(CommitTrigger::Enter));
    }

    #[test]
    fn negative_rejected_when_not_allowed() {
        let s = NumericEditSession::begin(120.0, bpm_format(), CommitPolicy::OnEnter);
        assert_eq!(s.commit("-5"), None);
        assert!(!s.is_committable("-5"));
    }

    #[test]
    fn drafts_in_native_digits_or_with_a_decimal_comma_parse() {
        let format = NumberFormat::decimal(20.0, 999.0, 2);
        assert_eq!(parse_draft("๑๒๐", &format), Some(120.0));
        assert_eq!(parse_draft("120,5", &format), Some(120.5));
        assert_eq!(parse_draft("１２０", &format), Some(120.0));
        // A comma next to a point is a thousands separator: still refused.
        assert_eq!(parse_draft("1,234.5", &format), None);
    }
}
