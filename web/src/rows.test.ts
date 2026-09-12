//! Row-layout tests for the browser shell's picker rows.
//!
//! The companion to `render.test.ts`: that one covers the *buffer* painter's row layout, this one
//! the picker's. `describePickerItem` is the whole projection from a wire `PickerItem` to what the
//! DOM builder then emits — which field leads, which is dim, what floats right — so a wire rename
//! that `tsc` cannot see (the types are hand-mirrored) shows up here as a row that lost its text.
//!
//! Deliberately not pixels, and not the DOM: the shapes worth pinning are the *parts* of a row and
//! the offsets that highlight them.

import { describe, expect, it } from "vitest";
import {
  agentRowBadge,
  composedTail,
  composedTailMatches,
  describePickerItem,
  formatElapsed,
  rowMatchSegments,
  shellRowBadge,
} from "./shell";
import type { PickerItem } from "./protocol";

const describe_ = (item: PickerItem) => describePickerItem(item, ["/w"], [""], 80);

const shell = (over: Partial<Extract<PickerItem, { kind: "shell" }>> = {}) =>
  ({
    kind: "shell",
    view_id: 12,
    title: "Shell 2",
    cwd: "~/proj",
    ...over,
  }) as PickerItem;

const agent = (over: Partial<Extract<PickerItem, { kind: "agent" }>> = {}) =>
  ({
    kind: "agent",
    view_id: 20,
    title: "Agent 1",
    agent: "Claude Code",
    ...over,
  }) as PickerItem;

describe("shell rows", () => {
  it("leads with the name and trails the directory and last command", () => {
    const d = describe_(shell({ last_command: "cargo test" }));
    expect(d.primary).toBe("Shell 2");
    expect(d.suffix).toBe("~/proj  cargo test");
  });

  it("floats a running badge to the right", () => {
    const d = describe_(shell({ last_command: "cargo test", running: true }));
    expect(d.metaParts).toEqual([{ text: "● running", cls: "picker-badge-running" }]);
  });

  it("shows a finished run's outcome and duration", () => {
    const ok = describe_(shell({ exit: 0, elapsed_ms: 3200 }));
    expect(ok.metaParts).toEqual([{ text: "✓ 0  3.2s", cls: "picker-badge-ok" }]);
    const bad = describe_(shell({ exit: 101, elapsed_ms: 1200 }));
    expect(bad.metaParts).toEqual([{ text: "✗ 101  1.2s", cls: "picker-badge-bad" }]);
  });

  it("a shell that has run nothing wears no badge, and neither does a dormant row", () => {
    expect(describe_(shell()).metaParts).toBeUndefined();
    expect(describe_(shell({ dormant: true, exit: 0, elapsed_ms: 1 })).metaParts).toBeUndefined();
    // …but the dormant row is dimmed, which is how "present, not loaded" reads.
    expect(describe_(shell({ dormant: true })).dim).toBe(true);
  });

  it("a killed run says so rather than claiming an exit code", () => {
    expect(shellRowBadge({ elapsed_ms: 1200 })).toEqual({
      text: "stopped  1.2s",
      cls: "picker-badge-muted",
    });
  });

  it("splits the haystack's match offsets across the name and the tail", () => {
    // "Shell 2  ~/proj  cargo test" — 0..6 name, 9..14 cwd, 17..26 command.
    const d = describe_(shell({ last_command: "cargo test", match_indices: [0, 9, 17] }));
    expect(d.matches).toEqual([0]); // 'S' of the name
    // The tail is "~/proj  cargo test": the cwd hit at 0, the command hit at 8.
    expect(d.suffixMatches).toEqual([0, 8]);
  });
});

describe("buffer rows", () => {
  const buffer = (over: Partial<Extract<PickerItem, { kind: "buffer" }>> = {}) =>
    ({
      kind: "buffer",
      buffer_id: 4,
      view_id: 4,
      display: "src/a.rs",
      ...over,
    }) as PickerItem;

  it("names a file at a revision by its path, with the commit dim and bracketed after it", () => {
    const d = describe_(buffer({ commit: "abc1234" }));
    expect(d.primary).toBe("src/a.rs");
    expect(d.suffix).toBe("(abc1234)");
  });

  it("splits the haystack's match offsets across the path and the commit", () => {
    // "src/a.rs  abc1234": 0..7 path, two-space join, then the hash from 10.
    const d = describe_(buffer({ commit: "abc1234", match_indices: [0, 10, 11] }));
    expect(d.matches).toEqual([0]); // 's' of the path
    // The rendered suffix is "(abc1234)", so the hash's own 0 and 1 sit past the bracket.
    expect(d.suffixMatches).toEqual([1, 2]); // 'a', 'b' of the hash
  });

  it("an ordinary buffer has no commit, and its offsets index the path alone", () => {
    const d = describe_(buffer({ match_indices: [0, 4] }));
    expect(d.suffix).toBeUndefined();
    expect(d.suffixMatches).toBeUndefined();
    expect(d.matches).toEqual([0, 4]);
  });
});

describe("agent rows", () => {
  it("leads with the name and trails the agent and last prompt", () => {
    const d = describe_(agent({ last_prompt: "fix the wrap bug" }));
    expect(d.primary).toBe("Agent 1");
    expect(d.suffix).toBe("Claude Code  fix the wrap bug");
  });

  it("badges what it is doing, and stays quiet when idle", () => {
    expect(describe_(agent()).metaParts).toBeUndefined();
    expect(describe_(agent({ state: { state: "idle" } })).metaParts).toBeUndefined();
    expect(describe_(agent({ state: { state: "thinking" } })).metaParts).toEqual([
      { text: "● thinking", cls: "picker-badge-running" },
    ]);
    expect(
      describe_(agent({ state: { state: "thinking", activity: "Reading src/lib.rs" } })).metaParts,
    ).toEqual([{ text: "● Reading src/lib.rs", cls: "picker-badge-running" }]);
    expect(describe_(agent({ state: { state: "awaiting_permission" } })).metaParts).toEqual([
      { text: "● awaiting permission", cls: "picker-badge-bad" },
    ]);
    expect(describe_(agent({ state: { state: "disconnected" } })).metaParts).toEqual([
      { text: "not connected", cls: "picker-badge-muted" },
    ]);
  });

  it("a dormant conversation shows no badge and no agent name it hasn't read", () => {
    const d = describe_(agent({ agent: "", dormant: true, state: { state: "disconnected" } }));
    expect(d.primary).toBe("Agent 1");
    expect(d.suffix).toBeUndefined();
    expect(agentRowBadge({ state: { state: "disconnected" }, dormant: true })).toBeUndefined();
  });
});

describe("the composed haystack", () => {
  it("elides an empty part, separator and all", () => {
    // "Shell 3  ~" — no command, so nothing sits after the directory.
    const seg = rowMatchSegments(["Shell 3", "~", ""], [0, 9]);
    expect(seg.first).toEqual([0]);
    expect(seg.second).toEqual([0]);
    expect(seg.third).toEqual([]);
    expect(composedTail(["Shell 3", "~", ""])).toBe("~");
    // A missing *middle* part shifts the third one down to the tail's start.
    expect(composedTail(["Shell 3", "", "cargo test"])).toBe("cargo test");
    const seg2 = rowMatchSegments(["Shell 3", "", "cargo test"], [9]);
    expect(seg2.third).toEqual([0]);
    expect(composedTailMatches(["Shell 3", "", "cargo test"], seg2)).toEqual([0]);
  });

  it("drops an index landing on a separator", () => {
    // Index 7 and 8 are the two spaces between "Shell 2" and "~/proj".
    const seg = rowMatchSegments(["Shell 2", "~/proj", ""], [7, 8]);
    expect(seg).toEqual({ first: [], second: [], third: [] });
  });
});

describe("elapsed formatting", () => {
  it("reads as quick / slow / very slow", () => {
    expect(formatElapsed(840)).toBe("840ms");
    expect(formatElapsed(3200)).toBe("3.2s");
    expect(formatElapsed(64000)).toBe("1m04s");
  });
});
