// WebSocket wrapper + protocol types for /v1/pty/ws
// Protocol version must match crates/hub/src/tunnel.rs::PROTOCOL_VERSION

export const PROTOCOL_VERSION = '7';

// `right` = vertical divider, new pane appears to the right (tmux `-h`).
// `down`  = horizontal divider, new pane appears below       (tmux `-v`).
export type SplitDirection = 'right' | 'down';

// Re-arrange every pane in the active session into one of tmux's
// even-* preset layouts. We deliberately expose just two — the common
// "everything side-by-side" / "everything stacked" choice — and leave
// main-* / tiled for later if there's demand.
export type PaneLayout = 'side_by_side' | 'stacked';

// ── Client → Hub ────────────────────────────────────────────────────────────

export type ClientMsg =
  | { type: 'hello'; token: string; version: string }
  | { type: 'list_agents' }
  | { type: 'select_agent'; agent: string | null }
  | { type: 'list_workspaces' }
  | { type: 'create_workspace'; name: string }
  | { type: 'delete_workspace'; name: string }
  | { type: 'reset_workspace'; name: string }
  | { type: 'open_session'; workspace: string; cols: number; rows: number; claude_args?: string[]; tool?: string }
  | { type: 'split_pane'; tool: string; direction: SplitDirection; args?: string[] }
  | { type: 'change_layout'; layout: PaneLayout }
  | { type: 'resize'; cols: number; rows: number }
  | { type: 'close' }
  | { type: 'pong' };

// ── Hub → Client ────────────────────────────────────────────────────────────

export type AgentItem = {
  name: string;
  current: boolean;
  /** Tools this agent reports as available. Empty when the agent
   *  is pre-v1.13 and didn't send the field; callers should treat
   *  that as "unknown, fall back to KNOWN_TOOLS" so we don't lock
   *  old agents out of every tool option. */
  tools: string[];
};
export type WorkspaceItem = { name: string; tmux_alive: boolean; has_client: boolean };

export type HubMsg =
  | { type: 'welcome'; account: string }
  | { type: 'rejected'; reason: string }
  | { type: 'agent_list'; items: AgentItem[] }
  | { type: 'agent_selected'; agent: string }
  | { type: 'workspace_list'; items: WorkspaceItem[] }
  | { type: 'workspace_created'; name: string }
  | { type: 'workspace_deleted'; name: string }
  | { type: 'workspace_reset'; name: string }
  | { type: 'session_opened'; agent: string; workspace: string; cwd: string }
  | { type: 'session_error'; message: string }
  | { type: 'session_closed'; reason?: string }
  | { type: 'ping' };

// ── WireSocket ──────────────────────────────────────────────────────────────

export type WireHandlers = {
  onMessage: (msg: HubMsg) => void;
  onBinary: (data: Uint8Array) => void;
  onClose: (code: number, reason: string) => void;
  onError: () => void;
};

export class WireSocket {
  private ws: WebSocket | null = null;
  private handlers: WireHandlers;

  constructor(handlers: WireHandlers) {
    this.handlers = handlers;
  }

  connect(): void {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${proto}//${location.host}/v1/pty/ws`;
    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    this.ws = ws;

    ws.onopen = () => {
      // Send Hello immediately; cookie auth lets us send empty token.
      this.send({ type: 'hello', token: '', version: PROTOCOL_VERSION });
    };

    ws.onmessage = (ev) => {
      if (typeof ev.data === 'string') {
        try {
          const msg = JSON.parse(ev.data) as HubMsg;
          if (msg.type === 'ping') {
            this.send({ type: 'pong' });
            return;
          }
          this.handlers.onMessage(msg);
        } catch {
          // ignore malformed frames
        }
      } else {
        // Binary: PTY output
        const buf = ev.data instanceof ArrayBuffer ? ev.data : (ev.data as Blob);
        if (buf instanceof ArrayBuffer) {
          this.handlers.onBinary(new Uint8Array(buf));
        }
      }
    };

    ws.onclose = (ev) => {
      this.ws = null;
      this.handlers.onClose(ev.code, ev.reason);
    };

    ws.onerror = () => {
      this.handlers.onError();
    };
  }

  send(msg: ClientMsg): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  sendBinary(data: Uint8Array): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(data);
    }
  }

  close(): void {
    if (this.ws) {
      try {
        this.send({ type: 'close' });
      } catch {
        // ignore
      }
      this.ws.close();
      this.ws = null;
    }
  }

  get connected(): boolean {
    return this.ws?.readyState === WebSocket.OPEN;
  }
}
