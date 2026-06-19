//! Increment/decrement the number at or after the cursor (`Ctrl-e` / `Ctrl-Alt-e`).
//!
//! A self-contained scan over a single line's chars: find the decimal integer the cursor sits on
//! (or the first one after it on the line), then re-render it shifted by `delta`. Vim's
//! `Ctrl-A`/`Ctrl-X` semantics — an immediately-preceding `-` is part of the number, and a number
//! written with a leading zero keeps its field width (`007` → `008`, `100` → `99`). The handler
//! leaves the whole result selected, so the selection follows the new digit count.
//!
//! When a selection is active the caller uses [`adjust_exact`] instead: it shifts only the selected
//! text, and only when that text is a strictly valid integer — a partial number adjusts just the
//! selected part, and a non-numeric selection is left untouched.

/// A located number within a line: the char range `[start, end)` relative to the line start and the
/// rendered replacement text shifted by the requested delta.
#[derive(Debug, PartialEq, Eq)]
pub struct NumberEdit {
    /// Char offset of the number's first char (the `-` sign, if any, else the first digit),
    /// relative to the line start.
    pub start: usize,
    /// Char offset one past the number's last digit, relative to the line start.
    pub end: usize,
    /// The shifted number, formatted with the original sign / field-width rules.
    pub text: String,
}

/// Find the integer the cursor sits on, or the first one at or after `from` (a char column within
/// `line`), and render it shifted by `delta`. Scanning never crosses the line. Returns `None` when
/// there's no digit at or after `from` — a no-op.
pub fn adjust(line: &str, from: usize, delta: i64) -> Option<NumberEdit> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();

    // First digit at or after the scan start.
    let mut i = from.min(n);
    while i < n && !chars[i].is_ascii_digit() {
        i += 1;
    }
    if i >= n {
        return None;
    }

    // Expand over the full digit run (the cursor may have landed in its middle).
    let mut digit_start = i;
    while digit_start > 0 && chars[digit_start - 1].is_ascii_digit() {
        digit_start -= 1;
    }
    let mut end = i;
    while end < n && chars[end].is_ascii_digit() {
        end += 1;
    }

    // A `-` hugging the digits is a sign — unless it's itself preceded by a digit, in which case
    // it's a subtraction operator (the `-` in `5-3` is not part of `3`).
    let mut start = digit_start;
    let mut negative = false;
    if digit_start > 0
        && chars[digit_start - 1] == '-'
        && !(digit_start >= 2 && chars[digit_start - 2].is_ascii_digit())
    {
        negative = true;
        start = digit_start - 1;
    }

    let digits: String = chars[digit_start..end].iter().collect();
    let text = shift(&digits, negative, delta);

    Some(NumberEdit { start, end, text })
}

/// Shift `s` by `delta` as an exact integer (the active-selection path). Returns `None` unless `s`
/// is a strictly valid integer — an optional leading `-` then one or more ASCII digits and nothing
/// else — so a non-numeric selection is left untouched.
pub fn adjust_exact(s: &str, delta: i64) -> Option<String> {
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(shift(digits, negative, delta))
}

/// Shift a digit run (already validated, with `negative` carrying the sign) by `delta` and render
/// it back with the original sign and field-width rules.
fn shift(digits: &str, negative: bool, delta: i64) -> String {
    // The run can exceed i64; saturate rather than panic on absurd input.
    let magnitude: i64 = digits.parse().unwrap_or(i64::MAX);
    let value = if negative { -magnitude } else { magnitude };
    let new_value = value.saturating_add(delta);
    // Preserve the field width only when the original was zero-padded (Vim: `007`+1 → `008`, but
    // `100`-1 → `99`).
    let pad = digits.len() > 1 && digits.starts_with('0');
    render(new_value, digits.len(), pad)
}

/// Render `value` as a (optionally zero-padded to `width` digits) decimal string with a leading
/// `-` when negative.
fn render(value: i64, width: usize, pad: bool) -> String {
    let body = value.unsigned_abs().to_string();
    let body = if pad && body.len() < width {
        format!("{}{}", "0".repeat(width - body.len()), body)
    } else {
        body
    };
    if value < 0 {
        format!("-{body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str, from: usize, delta: i64) -> NumberEdit {
        adjust(line, from, delta).expect("expected a number")
    }

    #[test]
    fn increments_number_under_cursor() {
        let e = ok("foo 42 bar", 4, 1);
        assert_eq!(
            e,
            NumberEdit {
                start: 4,
                end: 6,
                text: "43".into()
            }
        );
    }

    #[test]
    fn cursor_in_middle_of_run_takes_whole_number() {
        // Cursor on the '3' of 1234 still adjusts the whole number.
        let e = ok("v=1234;", 4, 1);
        assert_eq!((e.start, e.end, e.text.as_str()), (2, 6, "1235"));
    }

    #[test]
    fn scans_forward_to_first_number_on_line() {
        // Cursor before the number → Vim finds the next one on the line.
        let e = ok("  abc 7", 0, 1);
        assert_eq!((e.start, e.end, e.text.as_str()), (6, 7, "8"));
    }

    #[test]
    fn no_digit_after_cursor_is_a_noop() {
        assert_eq!(adjust("42 done", 2, 1), None);
        assert_eq!(adjust("no numbers here", 0, 1), None);
        assert_eq!(adjust("", 0, 1), None);
    }

    #[test]
    fn decrement_can_cross_zero_negative() {
        let e = ok("x 0", 2, -1);
        assert_eq!(e.text, "-1");
        let e = ok("x -1", 2, 1);
        assert_eq!((e.start, e.end, e.text.as_str()), (2, 4, "0"));
    }

    #[test]
    fn leading_minus_is_part_of_the_number() {
        let e = ok("val -5", 4, -1);
        assert_eq!((e.start, e.end, e.text.as_str()), (4, 6, "-6"));
    }

    #[test]
    fn minus_after_digit_is_subtraction_not_a_sign() {
        // In `5-3`, decrementing the `3` yields `2`, not `-4` — the `-` belongs to subtraction.
        let e = ok("5-3", 2, -1);
        assert_eq!((e.start, e.end, e.text.as_str()), (2, 3, "2"));
    }

    #[test]
    fn preserves_zero_padded_width() {
        assert_eq!(ok("007", 0, 1).text, "008");
        assert_eq!(ok("099", 0, 1).text, "100");
        assert_eq!(ok("010", 0, -1).text, "009");
        assert_eq!(ok("00", 0, 1).text, "01");
    }

    #[test]
    fn unpadded_number_keeps_no_padding() {
        // No leading zero → width is not forced.
        assert_eq!(ok("100", 0, -1).text, "99");
        assert_eq!(ok("9", 0, 1).text, "10");
    }

    #[test]
    fn counted_delta_applies_in_one_step() {
        assert_eq!(ok("n 10", 2, 5).text, "15");
        assert_eq!(ok("n 10", 2, -15).text, "-5");
    }

    #[test]
    fn adjust_exact_shifts_valid_integers() {
        assert_eq!(adjust_exact("42", 1).as_deref(), Some("43"));
        assert_eq!(adjust_exact("-5", 1).as_deref(), Some("-4"));
        assert_eq!(adjust_exact("0", -1).as_deref(), Some("-1"));
        // Same width / sign rules as the scan path.
        assert_eq!(adjust_exact("007", 1).as_deref(), Some("008"));
        assert_eq!(adjust_exact("100", -1).as_deref(), Some("99"));
        // A partial run is just a smaller integer (e.g. `23` selected out of `1234`).
        assert_eq!(adjust_exact("23", 1).as_deref(), Some("24"));
    }

    #[test]
    fn adjust_exact_rejects_non_integers() {
        for s in ["", "-", "4a", "12 34", " 5", "5 ", "+5", "1.5", "0x1f"] {
            assert_eq!(adjust_exact(s, 1), None, "{s:?} should be rejected");
        }
    }
}
