//! Project-root display labels (mirrors the TUI's labels.rs). The status bar and the picker prefix
//! paths with a disambiguating root label when the project has several roots that would otherwise
//! read the same.

/** One label per root, aligned by index. Single root → "" (no prefix needed). Roots that share a
 *  basename grow a parenthesized parent (`api (work)`), deepening until unique. */
export function rootLabels(paths: string[]): string[] {
  const n = paths.length;
  if (n === 0) return [];
  if (n === 1) return [""];
  const MAX_DEPTH = 16;
  const depths = new Array<number>(n).fill(0);
  const labels = new Array<string>(n).fill("");
  for (let pass = 0; pass <= MAX_DEPTH; pass++) {
    for (let i = 0; i < n; i++) labels[i] = labelAtDepth(paths[i], depths[i]);
    const byLabel = new Map<string, number[]>();
    labels.forEach((l, i) => {
      const arr = byLabel.get(l);
      if (arr) arr.push(i);
      else byLabel.set(l, [i]);
    });
    const collisions = [...byLabel.values()].filter((idxs) => idxs.length > 1);
    if (collisions.length === 0) return labels;
    let bumped = false;
    for (const idxs of collisions)
      for (const i of idxs)
        if (depths[i] < MAX_DEPTH) {
          depths[i]++;
          bumped = true;
        }
    if (!bumped) return labels; // every colliding entry maxed out — accept the duplicates
  }
  return labels;
}

/** `{basename}` at depth 0, else `{basename} ({parent…/})` with `depth` parent components. */
function labelAtDepth(path: string, depth: number): string {
  const comps = path.split("/").filter((c) => c.length > 0);
  const base = comps.length ? comps[comps.length - 1] : path;
  if (depth === 0) return base;
  const parents: string[] = [];
  for (let i = comps.length - 2; i >= 0 && parents.length < depth; i--) parents.push(comps[i]);
  if (parents.length === 0) return base;
  parents.reverse();
  return `${base} (${parents.join("/")})`;
}
