//! The browser's LSP dot classification (`lspDotClass`) — its copy of the core's `LspDot`. The
//! fold of lifecycle state and `$/progress` lives in this one function for the status bar, the
//! picker rows and the info dialog, so the cases pinned here are the cases all three paint.

import { describe, expect, it } from "vitest";
import { lspDotClass } from "./icons";
import type { LspProgress, LspStatus } from "./protocol";

const indexing: LspProgress[] = [{ title: "Indexing" }];
const dot = (status: LspStatus, progress?: LspProgress[]) => lspDotClass({ status, progress });

describe("lspDotClass", () => {
  it("ready is only the idle dot; work in flight is busy", () => {
    expect(dot({ state: "ready" })).toBe("lsp-ready");
    expect(dot({ state: "ready" }, [])).toBe("lsp-ready");
    expect(dot({ state: "ready" }, indexing)).toBe("lsp-busy");
  });

  it("coming up is busy, with or without progress", () => {
    for (const state of ["starting", "initializing", "restarting"] as const) {
      expect(dot({ state })).toBe("lsp-busy");
      expect(dot({ state }, indexing)).toBe("lsp-busy");
    }
  });

  it("crashed, missing and stopped are themselves, progress or not", () => {
    expect(dot({ state: "crashed", message: "boom" }, indexing)).toBe("lsp-crashed");
    expect(dot({ state: "missing", command: "gopls" })).toBe("lsp-missing");
    expect(dot({ state: "stopped" })).toBe("lsp-stopped");
  });
});
