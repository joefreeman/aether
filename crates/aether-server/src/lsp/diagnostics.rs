//! Convert LSP `publishDiagnostics` payloads into buffer-relative diagnostics.
//!
//! The server stores diagnostics in the buffer's own coordinates (line + byte column), so the render
//! path can slice them per line without re-touching LSP position encodings. The conversion from the
//! server's negotiated encoding to byte columns happens once, here.

use aether_protocol::viewport::DiagnosticSeverity;
use aether_protocol::LogicalPosition;
use ropey::Rope;
use serde_json::Value;

use super::position::{self, PositionEncoding};

/// A diagnostic resolved into the buffer's coordinates: `start`/`end` carry byte columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferDiagnostic {
    pub start: LogicalPosition,
    pub end: LogicalPosition,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

/// Convert an LSP `diagnostics` array (from `publishDiagnostics`) into buffer-relative diagnostics,
/// mapping each `character` from `encoding` to a byte column against `text`. Diagnostics whose line
/// is past the end of the buffer are dropped.
pub fn from_lsp(diagnostics: &Value, text: &Rope, encoding: PositionEncoding) -> Vec<BufferDiagnostic> {
    let Some(arr) = diagnostics.as_array() else {
        return Vec::new();
    };
    // Drop exact duplicates. Servers (rust-analyzer especially) sometimes publish the same
    // diagnostic twice in one batch — e.g. a file pulled in by multiple crates, or proc-macro
    // expansion — and a verbatim repeat is never useful. Dedup here so every surface (squiggles,
    // gutter counts, the diagnostics picker, the `Space j` hover) shows each once. Distinct
    // diagnostics that merely read alike differ in range and are kept. O(n²), but n is small.
    let mut out: Vec<BufferDiagnostic> = Vec::new();
    for d in arr.iter().filter_map(|d| convert_one(d, text, encoding)) {
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

fn convert_one(d: &Value, text: &Rope, encoding: PositionEncoding) -> Option<BufferDiagnostic> {
    let range = d.get("range")?;
    let start = lsp_pos_to_buffer(range.get("start")?, text, encoding)?;
    let end = lsp_pos_to_buffer(range.get("end")?, text, encoding)?;
    // LSP severity: 1=Error 2=Warning 3=Information 4=Hint. Absent → treat as Warning.
    let severity = match d.get("severity").and_then(Value::as_u64) {
        Some(1) => DiagnosticSeverity::Error,
        Some(3) => DiagnosticSeverity::Information,
        Some(4) => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Warning,
    };
    let message = d
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(BufferDiagnostic {
        start,
        end,
        severity,
        message,
    })
}

fn lsp_pos_to_buffer(pos: &Value, text: &Rope, encoding: PositionEncoding) -> Option<LogicalPosition> {
    let line = pos.get("line")?.as_u64()? as u32;
    let character = pos.get("character")?.as_u64()? as u32;
    if line as usize >= text.len_lines() {
        return None;
    }
    let line_str = line_text(text, line as usize);
    let col = position::lsp_to_byte(&line_str, character, encoding) as u32;
    Some(LogicalPosition { line, col })
}

fn line_text(text: &Rope, line: usize) -> String {
    let mut s: String = text.line(line).chunks().collect();
    if s.ends_with('\n') {
        s.pop();
    }
    if s.ends_with('\r') {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn diag(line0: u64, c0: u64, line1: u64, c1: u64, sev: u64, msg: &str) -> Value {
        json!({
            "range": {"start": {"line": line0, "character": c0}, "end": {"line": line1, "character": c1}},
            "severity": sev,
            "message": msg,
        })
    }

    #[test]
    fn converts_severity_and_range() {
        let text = Rope::from_str("fn main() {}\nlet x = 1;\n");
        let arr = json!([diag(1, 4, 1, 5, 1, "unused variable")]);
        let got = from_lsp(&arr, &text, PositionEncoding::Utf8);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].severity, DiagnosticSeverity::Error);
        assert_eq!(got[0].start, LogicalPosition { line: 1, col: 4 });
        assert_eq!(got[0].end, LogicalPosition { line: 1, col: 5 });
        assert_eq!(got[0].message, "unused variable");
    }

    #[test]
    fn drops_exact_duplicates_but_keeps_distinct_ranges() {
        let text = Rope::from_str("fn () {}\n");
        // Two verbatim "expected identifier" at the same zero-width point → one survives. A third
        // with the same message but a different range is a distinct diagnostic → kept.
        let arr = json!([
            diag(0, 3, 0, 3, 1, "expected identifier"),
            diag(0, 3, 0, 3, 1, "expected identifier"),
            diag(0, 5, 0, 5, 1, "expected identifier"),
        ]);
        let got = from_lsp(&arr, &text, PositionEncoding::Utf8);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].start.col, 3);
        assert_eq!(got[1].start.col, 5);
    }

    #[test]
    fn maps_utf16_columns_to_byte_columns() {
        // "héllo" — the diagnostic at UTF-16 char 3 ('l') is byte 4 (é is 2 bytes).
        let text = Rope::from_str("héllo\n");
        let arr = json!([diag(0, 3, 0, 4, 2, "x")]);
        let got = from_lsp(&arr, &text, PositionEncoding::Utf16);
        assert_eq!(got[0].start.col, 4);
        assert_eq!(got[0].end.col, 5);
        assert_eq!(got[0].severity, DiagnosticSeverity::Warning);
    }

    #[test]
    fn absent_severity_defaults_to_warning() {
        let text = Rope::from_str("x\n");
        let arr = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "message": "m"}]);
        let got = from_lsp(&arr, &text, PositionEncoding::Utf8);
        assert_eq!(got[0].severity, DiagnosticSeverity::Warning);
    }

    #[test]
    fn drops_out_of_range_lines() {
        let text = Rope::from_str("only one line\n");
        let arr = json!([diag(99, 0, 99, 1, 1, "off the end")]);
        assert!(from_lsp(&arr, &text, PositionEncoding::Utf8).is_empty());
    }

    #[test]
    fn empty_or_non_array_is_empty() {
        let text = Rope::from_str("x\n");
        assert!(from_lsp(&json!([]), &text, PositionEncoding::Utf8).is_empty());
        assert!(from_lsp(&json!(null), &text, PositionEncoding::Utf8).is_empty());
    }
}
