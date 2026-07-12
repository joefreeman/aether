//! Increment/decrement a number (`Ctrl-a` / `Ctrl-Alt-a`).
//!
//! Two operand modes, chosen by the caller (see `handlers::resolve_number_edit`):
//! - **Normal mode**: the caller passes the exact selected text (a point cursor being the one char
//!   under the block) to [`adjust_exact`]. There is no scan — the operand never grows beyond the
//!   selection, so an unselected `-` or neighbouring digit can't be swept in and invert the
//!   direction. A leading `-` *within* the selection is the number's sign.
//! - **Insert mode**: there's no selection, so [`find_number`] scans the caret's line for the
//!   number at/after the cursor (Vim `Ctrl-A`) and the caller adjusts that.
//!
//! Either way [`adjust_exact`] only shifts a strictly valid integer, and a number written with a
//! leading zero keeps its field width (`007` → `008`, `100` → `99`).

/// Scan `line` for the integer to adjust at or after char column `col` (Vim `Ctrl-A`): the digit
/// run containing the caret, or the next run after it, plus an immediately-preceding `-` sign (only
/// when that `-` isn't itself preceded by a digit, so the `-` in `1-2` reads as subtraction, not a
/// sign). Returns the operand's char range `[start, end)` within the line, or `None` when no number
/// sits at/after the caret. Used by Insert-mode adjust, where there's no selection to act on.
pub fn find_number(line: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < n && chars[end].is_ascii_digit() {
            end += 1;
        }
        // The caret is inside this run, or this is the first run after the caret.
        if end >= col {
            let signed_start = if start > 0
                && chars[start - 1] == '-'
                && !(start >= 2 && chars[start - 2].is_ascii_digit())
            {
                start - 1
            } else {
                start
            };
            return Some((signed_start, end));
        }
        i = end;
    }
    None
}

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

    #[test]
    fn find_number_grabs_the_run_under_the_caret() {
        // Caret anywhere inside (or just past) "123" → the whole run.
        for col in 0..=3 {
            assert_eq!(find_number("ab123cd", col + 2), Some((2, 5)), "col {col}");
        }
    }

    #[test]
    fn find_number_jumps_to_the_next_number_after_the_caret() {
        // Caret before the number → scan forward to it.
        assert_eq!(find_number("  42", 0), Some((2, 4)));
        // Caret past the first number → the second one.
        assert_eq!(find_number("1 22", 2), Some((2, 4)));
    }

    #[test]
    fn find_number_includes_a_leading_sign() {
        assert_eq!(find_number("x-5", 2), Some((1, 3)));
        // ...but a `-` after a digit is subtraction, not a sign: caret on the `2` grabs just "2".
        assert_eq!(find_number("1-2", 2), Some((2, 3)));
    }

    #[test]
    fn find_number_returns_none_when_no_number_at_or_after_caret() {
        assert_eq!(find_number("abc", 0), None);
        // The only number is entirely behind the caret.
        assert_eq!(find_number("12 ab", 3), None);
    }
}
