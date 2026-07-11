//! WebSocket JSON-RPC 2.0 client. Mirrors the TUI's client.rs split: request/response by id, plus
//! a notification callback for server-pushed messages (viewport/lines_changed, buffer/state, …).
//!
//! Reconnection: a dropped socket auto-reconnects with backoff. Because the server assigns a fresh
//! client_id per connection (so cursor/selection/undo/viewport/picker state is per-connection), the
//! app must re-bootstrap on reconnect — the client reports state transitions via `onConnState` and
//! fires `onReconnect` once a *re*connection (not the first connect) opens.

export type NotificationHandler = (method: string, params: unknown) => void;

/**
 * connecting/reconnecting are "down" states; connected is up; failed means we gave up retrying;
 * outdated means the daemon was replaced by a different build than served this page — terminal, and
 * only a reload (which fetches the fresh bundle) can recover.
 */
export type ConnState = "connecting" | "connected" | "reconnecting" | "failed" | "outdated";

export interface RpcClientOpts {
  onConnState: (state: ConnState) => void;
  /** Fired after a *re*connection opens (not the initial connect) — the app should re-bootstrap. */
  onReconnect: () => void;
  /** HTTP origin of the daemon (e.g. `http://127.0.0.1:2384`) for the out-of-band `/status` probe. */
  httpBase: string;
  /**
   * The version that served this page (from the `aether-version` meta tag), or `null` when absent
   * (the Vite dev server doesn't inject it). When set, a reconnect that finds the daemon reporting a
   * *different* version is treated as "outdated" rather than retried — the browser can't read the
   * handshake's 426, so we compare versions over `/status` instead.
   */
  buildVersion: string | null;
}

/** A JSON-RPC error response, carrying the numeric code so callers can branch (e.g. WOULD_OVERWRITE). */
export class RpcError extends Error {
  constructor(
    readonly code: number,
    readonly rpcMessage: string,
    readonly method: string,
  ) {
    super(`RPC ${method}: ${rpcMessage}`);
    this.name = "RpcError";
  }
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
  method: string;
}

const MAX_RECONNECT_ATTEMPTS = 12;

export class RpcClient {
  private ws!: WebSocket;
  private readonly url: string;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private readonly onNotification: NotificationHandler;
  private readonly onConnState: (state: ConnState) => void;
  private readonly onReconnect: () => void;
  private readonly httpBase: string;
  private readonly buildVersion: string | null;
  private firstConnect = true;
  private attempts = 0;
  private reconnectTimer: number | undefined;
  private resolveReady!: () => void;
  private rejectReady!: (err: Error) => void;
  /** Resolves when the *first* connection opens; rejects if the initial connect exhausts retries. */
  readonly ready: Promise<void>;

  constructor(url: string, onNotification: NotificationHandler, opts: RpcClientOpts) {
    this.url = url;
    this.onNotification = onNotification;
    this.onConnState = opts.onConnState;
    this.onReconnect = opts.onReconnect;
    this.httpBase = opts.httpBase;
    this.buildVersion = opts.buildVersion;
    this.ready = new Promise<void>((resolve, reject) => {
      this.resolveReady = resolve;
      this.rejectReady = reject;
    });
    this.connect();
  }

  private connect(): void {
    this.onConnState(this.firstConnect ? "connecting" : "reconnecting");
    const ws = new WebSocket(this.url);
    this.ws = ws;
    ws.addEventListener("open", () => {
      this.attempts = 0;
      this.onConnState("connected");
      if (this.firstConnect) {
        this.firstConnect = false;
        this.resolveReady();
      } else {
        this.onReconnect();
      }
    });
    ws.addEventListener("message", (ev) => this.handleMessage(ev.data));
    ws.addEventListener("close", () => this.handleClose());
    // `error` is always followed by `close`; let close drive reconnection.
    ws.addEventListener("error", () => {});
  }

  private handleClose(): void {
    const err = new Error("disconnected");
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();

    this.attempts += 1;
    if (this.attempts > MAX_RECONNECT_ATTEMPTS) {
      this.onConnState("failed");
      if (this.firstConnect) this.rejectReady(new Error("WebSocket connection failed"));
      return;
    }
    const delay = Math.min(10_000, 500 * 2 ** (this.attempts - 1));
    this.onConnState("reconnecting");
    this.reconnectTimer = window.setTimeout(() => void this.reconnect(), delay);
  }

  /** A reconnect attempt, gated on a version check. A dropped localhost socket usually means the
   *  daemon restarted; if it came back as a *different* build than served this page, the wire format
   *  may have drifted, so retrying can't help and silently reconnecting risks talking a skewed
   *  protocol. Surface "outdated" (→ reload) instead of looping. Server unreachable or same-version
   *  falls through to a normal reconnect. */
  private async reconnect(): Promise<void> {
    if (await this.serverOutdated()) {
      this.onConnState("outdated");
      if (this.firstConnect) this.rejectReady(new Error("server version changed — reload required"));
      return;
    }
    this.connect();
  }

  /** True only if the daemon is reachable AND reports a version different from the one that served
   *  this page. Unknown build version (dev), an unreachable server (still restarting), or a matching
   *  version all return false, so the normal retry loop continues. */
  private async serverOutdated(): Promise<boolean> {
    if (!this.buildVersion) return false;
    try {
      const resp = await fetch(`${this.httpBase}/status`, { cache: "no-store" });
      if (!resp.ok) return false;
      const status = (await resp.json()) as { version?: unknown };
      return typeof status.version === "string" && status.version !== this.buildVersion;
    } catch {
      return false; // unreachable — treat as down, keep retrying
    }
  }

  /** Manual retry after we've given up ("failed"). */
  retry(): void {
    window.clearTimeout(this.reconnectTimer);
    this.attempts = 0;
    void this.reconnect();
  }

  private handleMessage(data: unknown): void {
    if (typeof data !== "string") return;
    let msg: {
      id?: number;
      method?: string;
      result?: unknown;
      error?: { code?: number; message?: string };
      params?: unknown;
    };
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    if (typeof msg.id === "number") {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      if (msg.error) p.reject(new RpcError(msg.error.code ?? 0, msg.error.message ?? "error", p.method));
      else p.resolve(msg.result);
    } else if (typeof msg.method === "string") {
      // Never let a single bad/unexpected push tear down message handling for the connection.
      try {
        this.onNotification(msg.method, msg.params);
      } catch (e) {
        console.error("notification handler failed", msg.method, e);
      }
    }
  }

  rpc<R>(method: string, params: unknown): Promise<R> {
    if (this.ws.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("disconnected"));
    }
    const id = this.nextId++;
    const req = { jsonrpc: "2.0", id, method, params };
    return new Promise<R>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, method });
      this.ws.send(JSON.stringify(req));
    });
  }
}
