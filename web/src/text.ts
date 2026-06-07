//! UTF-8 ↔ UTF-16 conversion. The protocol uses UTF-8 byte offsets for highlights, diagnostics,
//! search matches, and cursor columns; JS strings are UTF-16. Everything that maps a wire byte
//! offset to a position in a JS string goes through here (docs/web-client.md §2.3).

const encoder = new TextEncoder();

/** Number of UTF-8 bytes a single code point occupies. */
function utf8LenOfCodePoint(cp: number): number {
  if (cp < 0x80) return 1;
  if (cp < 0x800) return 2;
  if (cp < 0x10000) return 3;
  return 4;
}

/** UTF-8 byte length of a string. */
export function utf8ByteLen(s: string): number {
  return encoder.encode(s).length;
}

export interface RowCodePoints {
  /** The string split into code points (so emoji / surrogate pairs are single units). */
  cps: string[];
  /** Cumulative UTF-8 byte offset before each code point; length is `cps.length + 1`, with the
   *  final entry equal to the row's total byte length. */
  byteStart: number[];
  byteLen: number;
}

/** Decode a row's text into code points plus a per-code-point byte offset table, so wire byte
 *  ranges can be mapped onto JS substrings without per-character TextEncoder calls. */
export function decodeRow(s: string): RowCodePoints {
  const cps: string[] = [];
  const byteStart: number[] = [0];
  let b = 0;
  for (const ch of s) {
    cps.push(ch);
    b += utf8LenOfCodePoint(ch.codePointAt(0)!);
    byteStart.push(b);
  }
  return { cps, byteStart, byteLen: b };
}
