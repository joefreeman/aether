//! Increment/decrement the selected number (`Ctrl-e` / `Ctrl-Alt-e`).
//!
//! Selection-only, single mode: the caller passes the exact selected text (a point cursor being the
//! one char under the block) and we shift it by `delta`, but only when that text is a strictly valid
//! integer. There is no line scan — the operand never grows beyond the selection, so an unselected
//! `-` or neighbouring digit can't be swept in and invert the direction. A leading `-` *within* the
//! selection is the number's sign, and a number written with a leading zero keeps its field width
//! (`007` → `008`, `100` → `99`). The handler leaves the whole result selected, so the selection
//! follows the new digit count.

/// Shift `s` by `delta` as an exact integer. Returns `None` unless `s` is a strictly valid integer —
/// an optional leading `-` then one or more ASCII digits and nothing else — so a non-numeric (or
/// partially-numeric) selection is left untouched.
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

    #[test]
    fn shifts_valid_integers() {
        assert_eq!(adjust_exact("42", 1).as_deref(), Some("43"));
        assert_eq!(adjust_exact("9", 1).as_deref(), Some("10"));
        // A partial run is just a smaller integer (e.g. `23` selected out of `1234`).
        assert_eq!(adjust_exact("23", 1).as_deref(), Some("24"));
    }

    #[test]
    fn selected_minus_is_the_sign() {
        // A `-` inside the selection is the number's sign; the value moves accordingly.
        assert_eq!(adjust_exact("-5", 1).as_deref(), Some("-4"));
        assert_eq!(adjust_exact("-5", -1).as_deref(), Some("-6"));
    }

    #[test]
    fn unselected_minus_stays_out() {
        // The selection is just the digit `5` (the `-` of `-5` isn't selected). It adjusts as a
        // bare `5`, so the direction never inverts — increment → `6`, not `-4`.
        assert_eq!(adjust_exact("5", 1).as_deref(), Some("6"));
        assert_eq!(adjust_exact("5", -1).as_deref(), Some("4"));
    }

    #[test]
    fn crosses_zero_into_negative() {
        assert_eq!(adjust_exact("0", -1).as_deref(), Some("-1"));
        assert_eq!(adjust_exact("-1", 1).as_deref(), Some("0"));
    }

    #[test]
    fn preserves_zero_padded_width() {
        assert_eq!(adjust_exact("007", 1).as_deref(), Some("008"));
        assert_eq!(adjust_exact("099", 1).as_deref(), Some("100"));
        assert_eq!(adjust_exact("010", -1).as_deref(), Some("009"));
        assert_eq!(adjust_exact("00", 1).as_deref(), Some("01"));
    }

    #[test]
    fn unpadded_number_keeps_no_padding() {
        // No leading zero → width is not forced.
        assert_eq!(adjust_exact("100", -1).as_deref(), Some("99"));
    }

    #[test]
    fn counted_delta_applies_in_one_step() {
        assert_eq!(adjust_exact("10", 5).as_deref(), Some("15"));
        assert_eq!(adjust_exact("10", -15).as_deref(), Some("-5"));
    }

    #[test]
    fn rejects_non_integers() {
        // No scanning: anything but a clean optional-sign-then-digits run is left untouched —
        // including a selection that merely *contains* a number (`5-3`, `a12b`).
        for s in [
            "", "-", "4a", "12 34", " 5", "5 ", "+5", "1.5", "0x1f", "5-3", "a12b",
        ] {
            assert_eq!(adjust_exact(s, 1), None, "{s:?} should be rejected");
        }
    }
}
