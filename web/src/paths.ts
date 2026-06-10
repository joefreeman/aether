//! Standardised path truncation — mirrors the TUI's `truncate_path_with_indices`
//! (crates/aether-tui/src/ui.rs) so paths shrink the same way in both clients. Budgets here
//! are in *characters*: web text is proportional, so call sites estimate a char budget from
//! pixels (see `charBudget`) and keep CSS `text-overflow: ellipsis` as the safety net for the
//! estimate's error margin.

/** Shrink `path` into `maxChars` through a segment-aware ladder:
 *
 *   1. Fits → unchanged.
 *   2. Elide whole *middle* segments to a single `…` (`crates/…/src/handlers.rs`): the last
 *      segment (the filename) always survives, and among fitting candidates we keep as many
 *      segments as possible, ties broken toward the tail — the file's parents identify it
 *      better than leading dirs do.
 *   3. Floor: char-level left-cut with a leading `…`, keeping the end of the string.
 *
 *  `matchIndices` (char offsets into `path`) are remapped into the display; indices falling
 *  inside an elided span drop out. Strings without `/` skip straight to the floor. */
export function truncatePath(
  path: string,
  matchIndices: number[] | undefined,
  maxChars: number,
): { display: string; indices: number[] | undefined } {
  if (maxChars <= 0) return { display: "", indices: matchIndices ? [] : undefined };
  const chars = [...path];
  if (chars.length <= maxChars) return { display: path, indices: matchIndices };

  // Rung 2: segment elision — keep the first `l` and last `t` segments around one `…` part;
  // pick the fitting candidate with the most segments, preferring tail on ties.
  const segs = path.split("/");
  const n = segs.length;
  if (n >= 2) {
    const segW = segs.map((s) => [...s].length);
    let best: { l: number; t: number } | null = null;
    for (let t = 1; t < n; t++) {
      for (let l = 0; l <= n - 1 - t; l++) {
        let w = l + t + 1; // one `/` per kept segment + the `…` itself
        for (let i = 0; i < l; i++) w += segW[i];
        for (let i = n - t; i < n; i++) w += segW[i];
        if (w <= maxChars && (best === null || l + t > best.l + best.t || (l + t === best.l + best.t && t > best.t))) {
          best = { l, t };
        }
      }
    }
    if (best !== null) {
      const { l, t } = best;
      const lead = segs.slice(0, l).join("/");
      const tail = segs.slice(n - t).join("/");
      const display = l === 0 ? `…/${tail}` : `${lead}/…/${tail}`;
      // Remap: the kept lead is an exact prefix of the original, the kept tail an exact
      // suffix; everything between (the elided span and its separators) drops out.
      const leadChars = [...lead].length;
      const origTailStart = chars.length - [...tail].length;
      const displayTailStart = l === 0 ? 2 : leadChars + 3; // past `…/` / `/…/`
      const indices = matchIndices
        ?.map((i) =>
          l > 0 && i < leadChars ? i : i >= origTailStart ? i - origTailStart + displayTailStart : -1,
        )
        .filter((i) => i >= 0);
      return { display, indices };
    }
  }

  // Rung 3 (floor): keep the last maxChars - 1 characters behind a leading `…`.
  const keptStart = chars.length - (maxChars - 1);
  const display = `…${chars.slice(keptStart).join("")}`;
  const indices = matchIndices
    ?.filter((i) => i >= keptStart)
    .map((i) => i - keptStart + 1);
  return { display, indices };
}

/** Estimate how many characters fit into `px` of the given font, using a one-off canvas
 *  measurement of a representative sample (cached per font string). Slightly conservative —
 *  better to elide one segment too many than to let the CSS safety-net ellipsis eat the
 *  filename we worked to keep. */
const avgCharW = new Map<string, number>();
export function charBudget(px: number, font: string): number {
  let avg = avgCharW.get(font);
  if (avg === undefined) {
    const ctx = document.createElement("canvas").getContext("2d");
    if (!ctx) return Math.floor(px / 8);
    ctx.font = font;
    const sample = "crates/aether-server_handlers.rs 0123456789";
    avg = ctx.measureText(sample).width / [...sample].length;
    avgCharW.set(font, avg);
  }
  return Math.floor((px / avg) * 0.95);
}
