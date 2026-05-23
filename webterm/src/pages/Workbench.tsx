// IDE-style workbench: left sidebar (agent tree) + right tab bar + xterm area.
// Owns:
//   1. Control WS — menu phase (list agents / workspaces, create/delete/reset)
//   2. Per-tab PTY WS — one independent WireSocket + Terminal per open workspace

import {
  useEffect,
  useRef,
  useState,
  useCallback,
  useReducer,
} from 'react';
import { useNavigate } from 'react-router-dom';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { CanvasAddon } from '@xterm/addon-canvas';
import { WebglAddon } from '@xterm/addon-webgl';
import '@xterm/xterm/css/xterm.css';

import { apiClient } from '@/lib/api';
import {
  WireSocket,
  type AgentItem,
  type WorkspaceItem,
  type HubMsg,
  type PaneLayout,
  type SplitDirection,
} from '@/lib/wire';
import { effectiveTheme, getStoredTheme, type Theme } from '@/lib/theme';
import { type Tab, tabKey } from '@/lib/tabs';
import {
  DEFAULT_PREFERENCES,
  parsePreferences,
  serializePreferences,
  type Preferences,
} from '@/lib/preferences';
import type { Tool } from '@/lib/tools';
import { DEFAULT_TOOL, KNOWN_TOOLS } from '@/lib/tools';
import Sidebar from '@/components/Sidebar';
import TabBar from '@/components/TabBar';
import SettingsDialog from '@/components/SettingsDialog';

// ── xterm theme helpers ──────────────────────────────────────────────────────

function darkXterm() {
  return { background: '#18181b', foreground: '#fafafa', cursor: '#fafafa' };
}

function lightXterm() {
  return { background: '#ffffff', foreground: '#18181b', cursor: '#18181b' };
}

function xtermTheme(dark: boolean) {
  return dark ? darkXterm() : lightXterm();
}

// ── Tab state reducer ────────────────────────────────────────────────────────

type TabAction =
  | { type: 'ADD'; tab: Tab }
  | { type: 'UPDATE'; id: string; patch: Partial<Tab> }
  | { type: 'REMOVE'; id: string };

function tabsReducer(state: Tab[], action: TabAction): Tab[] {
  switch (action.type) {
    case 'ADD':
      return [...state, action.tab];
    case 'UPDATE':
      return state.map((t) => (t.id === action.id ? { ...t, ...action.patch } : t));
    case 'REMOVE':
      return state.filter((t) => t.id !== action.id);
    default:
      return state;
  }
}

// ── Workbench ────────────────────────────────────────────────────────────────

export default function Workbench() {
  const navigate = useNavigate();

  // Auth
  const [account, setAccount] = useState('');
  const [authLoading, setAuthLoading] = useState(true);

  // Control WS (menu phase)
  const ctrlWsRef = useRef<WireSocket | null>(null);
  const [ctrlReady, setCtrlReady] = useState(false);

  // Agent tree data
  const [agents, setAgents] = useState<AgentItem[]>([]);
  const [agentsLoading, setAgentsLoading] = useState(true);
  const [workspaces, setWorkspaces] = useState<WorkspaceItem[]>([]);

  // Currently "selected" agent on the control connection (needed for create/list)
  const ctrlAgentRef = useRef<string | null>(null);

  // Tabs
  const [tabs, dispatchTabs] = useReducer(tabsReducer, []);
  const tabsRef = useRef<Tab[]>(tabs);
  tabsRef.current = tabs;
  const [activeTabId, setActiveTabId] = useState<string | null>(null);

  // Settings dialog
  const [showSettings, setShowSettings] = useState(false);

  // Per-user preferences (default args per tool, future things). Loaded
  // from the hub on mount; kept in a ref so non-reactive callbacks
  // (handleTabMsg, handleSplit) see fresh values without re-binding.
  const [preferences, setPreferences] = useState<Preferences>(DEFAULT_PREFERENCES);
  const preferencesRef = useRef<Preferences>(preferences);
  preferencesRef.current = preferences;

  // Transient error toasts (e.g. split-pane failures). SessionError is a
  // non-fatal hub event by design, so we surface it inline instead of
  // tearing down the user's tab.
  type Toast = { id: string; message: string };
  const [toasts, setToasts] = useState<Toast[]>([]);
  const addToast = useCallback((message: string) => {
    const id = crypto.randomUUID();
    setToasts((prev) => [...prev, { id, message }]);
    setTimeout(() => {
      setToasts((prev) => prev.filter((t) => t.id !== id));
    }, 6000);
  }, []);
  const dismissToast = useCallback((id: string) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }, []);

  // Refresh timer ref (30s poll)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Container DOM nodes per tab id. Kept outside of React state on
  // purpose — touching them during a ref callback must NOT trigger
  // a re-render, or the inline ref creates an infinite loop.
  const containersRef = useRef<Map<string, HTMLDivElement>>(new Map());

  // Per-tab "tmux is sitting in copy-mode with a live selection"
  // flag, used by the Cmd+C bridge. Lives outside React state
  // because the tabs reducer rebuilds tab objects on every UPDATE
  // (e.g. status transitions), which would otherwise discard a
  // mutation we just made on the old tab reference.
  const copyModeRef = useRef<Map<string, boolean>>(new Map());

  // ── Auth check ─────────────────────────────────────────────────────────────

  useEffect(() => {
    apiClient
      .me()
      .then((me) => {
        setAccount(me.account);
        setAuthLoading(false);
      })
      .catch(() => {
        navigate('/login', { replace: true });
      });
  }, [navigate]);

  // ── Preferences load ─────────────────────────────────────────────────────
  // Fire-and-forget once we know the user is authed. Failures are
  // non-fatal: webterm just keeps the in-memory defaults until the user
  // either retries or saves.

  useEffect(() => {
    if (authLoading) return;
    apiClient
      .getPreferences()
      .then((resp) => setPreferences(parsePreferences(resp.preferences)))
      .catch(() => {
        // Network blip on first paint — stick with defaults so the
        // session-open flow keeps working.
      });
  }, [authLoading]);

  const savePreferences = useCallback(async (next: Preferences) => {
    setPreferences(next);
    try {
      await apiClient.putPreferences(serializePreferences(next));
    } catch {
      addToast('Could not save preferences — your change applies to this tab but did not persist.');
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ── Control WS helpers ─────────────────────────────────────────────────────

  // v1.13: list_workspaces returns ALL workspaces across agents in
  // one frame. No more select_agent dance — we just ask the hub
  // and group by item.agent client-side for the tree UI.
  const refreshWorkspaces = useCallback(() => {
    if (!ctrlWsRef.current?.connected) return;
    ctrlWsRef.current.send({ type: 'list_workspaces' });
  }, []);

  const schedulePoll = useCallback(() => {
    if (pollTimerRef.current) clearTimeout(pollTimerRef.current);
    pollTimerRef.current = setTimeout(() => {
      refreshWorkspaces();
      schedulePoll();
    }, 30_000);
  }, [refreshWorkspaces]);

  // ── Control WS message handler ─────────────────────────────────────────────

  const handleCtrlMsg = useCallback(
    (msg: HubMsg) => {
      switch (msg.type) {
        case 'welcome':
          setCtrlReady(true);
          ctrlWsRef.current?.send({ type: 'list_agents' });
          ctrlWsRef.current?.send({ type: 'list_workspaces' });
          break;

        case 'agent_list':
          setAgents(msg.items);
          setAgentsLoading(false);
          break;

        case 'agent_selected':
          // Legacy frame; we no longer drive the picker via
          // select_agent. Tabs may still trigger one via the
          // session-open path (line ~561). Keep ctrlAgentRef in
          // sync for any code that still reads it, but don't
          // re-fetch — the cross-agent list is already up to date.
          ctrlAgentRef.current = msg.agent;
          break;

        case 'workspace_list':
          // v1.13: flat cross-agent list — pass through directly.
          setWorkspaces(msg.items);
          break;

        case 'workspace_created':
        case 'workspace_deleted':
        case 'workspace_reset':
          // Single round-trip refresh; no agent context needed.
          ctrlWsRef.current?.send({ type: 'list_workspaces' });
          break;

        case 'rejected':
          // Control WS rejected — most likely session expired
          navigate('/login', { replace: true });
          break;

        default:
          break;
      }
    },
    [navigate],
  );

  // ── Build control WS ───────────────────────────────────────────────────────

  useEffect(() => {
    if (authLoading) return;

    const ws = new WireSocket({
      onMessage: handleCtrlMsg,
      onBinary: () => {},
      onClose: () => {
        setCtrlReady(false);
      },
      onError: () => {
        setCtrlReady(false);
      },
    });

    // Patch handlers to intercept Welcome so we can set ctrlReady + list agents
    // handleCtrlMsg already handles 'welcome', so just connect directly.
    ws.connect();
    ctrlWsRef.current = ws;
    schedulePoll();

    return () => {
      if (pollTimerRef.current) clearTimeout(pollTimerRef.current);
      ws.close();
      ctrlWsRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [authLoading]);

  // ── Sidebar callbacks ──────────────────────────────────────────────────────

  function handleCreateWorkspace(agent: string, name: string) {
    if (!ctrlWsRef.current?.connected) return;
    ctrlWsRef.current.send({ type: 'create_workspace', name, agent });
  }

  // The hub holds a per-workspace mutex: as long as some session is
  // attached it refuses delete/reset with "workspace is currently in
  // use". For the web UI that means a workspace with an open tab can
  // never be cleaned up. Close the tab first, let the WS-close
  // propagate so the hub's mutex clears, then fire the menu-level
  // request from the control WS.
  function withTabClosed(
    agent: string,
    workspace: string,
    fire: () => void,
  ) {
    const key = tabKey(agent, workspace);
    const openTab = tabsRef.current.find(
      (t) => tabKey(t.agent, t.workspace) === key,
    );
    if (openTab) {
      closeTabRef.current(openTab.id);
      // Empirically the hub releases its workspace mutex once the WS
      // close handshake completes. 400 ms is a safe budget; if we
      // see flakiness we can bump it or wait on a real ack.
      setTimeout(fire, 400);
    } else {
      fire();
    }
  }

  function handleResetWorkspace(agent: string, workspace: string) {
    if (!ctrlWsRef.current?.connected) return;
    withTabClosed(agent, workspace, () => {
      ctrlWsRef.current?.send({ type: 'reset_workspace', name: workspace, agent });
    });
  }

  function handleDeleteWorkspace(agent: string, workspace: string) {
    if (!ctrlWsRef.current?.connected) return;
    withTabClosed(agent, workspace, () => {
      ctrlWsRef.current?.send({ type: 'delete_workspace', name: workspace, agent });
    });
  }

  // ── Open tab ──────────────────────────────────────────────────────────────

  const openTab = useCallback(
    (agent: string, workspace: string, tool?: string) => {
      // Deduplicate by agent::workspace (tab is reused regardless of tool)
      const key = tabKey(agent, workspace);
      const existing = tabsRef.current.find(
        (t) => tabKey(t.agent, t.workspace) === key,
      );
      if (existing) {
        setActiveTabId(existing.id);
        // Mirror the focus + fit that `selectTab` does so clicking a
        // sidebar row for an already-open tab lands the cursor in
        // xterm — without this the user has to click the terminal
        // pane once more before typing.
        requestAnimationFrame(() => {
          if (!containersRef.current.has(existing.id)) return;
          try {
            existing.fitAddon.fit();
            if (existing.ws.connected) {
              existing.ws.send({
                type: 'resize',
                cols: existing.term.cols,
                rows: existing.term.rows,
              });
            }
          } catch {
            // ignore
          }
          existing.term.focus();
        });
        return;
      }

      const isDark = effectiveTheme(getStoredTheme()) === 'dark';
      const term = new Terminal({
        cursorBlink: true,
        scrollback: 10000,
        fontFamily: 'ui-monospace, Menlo, Monaco, monospace',
        fontSize: 14,
        theme: xtermTheme(isDark),
        // Default is true, which interprets "any user input" — *including
        // a plain click on the terminal pane* — as a signal to snap the
        // viewport to the bottom. That ruined the UX of scrolling up
        // through claude's history: the moment you click anywhere to,
        // say, start a selection, you're yanked back to the end. New PTY
        // output still scrolls automatically (xterm respects the user's
        // current scroll position only while they're scrolled up — once
        // the viewport is at the bottom new bytes push it along).
        scrollOnUserInput: false,
        // Higher = more rows per wheel tick. Default 1 feels sluggish
        // on Mac trackpads where each tick is small but frequent;
        // 3 covers a chat exchange in roughly one swipe without
        // overshooting on a precise scroll.
        scrollSensitivity: 3,
      });
      const fitAddon = new FitAddon();
      const linksAddon = new WebLinksAddon();
      term.loadAddon(fitAddon);
      term.loadAddon(linksAddon);
      // Renderer ladder: WebGL → Canvas → DOM (xterm's built-in).
      // WebGL is the fastest by a wide margin (GPU-backed texture
      // atlas, batched draws) and is the only one that keeps up
      // smoothly when claude streams a long response while the user
      // scrolls the buffer. Canvas is a usable middle ground; the
      // built-in DOM renderer is the last resort.
      //
      // The WebGL addon can fail or its GL context can be lost
      // mid-session (Safari with memory pressure, certain GPU
      // drivers, virtualized displays). We register a
      // `contextLoss` handler and demote to Canvas in place
      // instead of leaving the user with a broken / blank pane.
      try {
        const webgl = new WebglAddon();
        webgl.onContextLoss(() => {
          webgl.dispose();
          try {
            term.loadAddon(new CanvasAddon());
          } catch {
            // Final fallback is xterm's DOM renderer, which is
            // always present and doesn't need loading.
          }
        });
        term.loadAddon(webgl);
      } catch {
        try {
          term.loadAddon(new CanvasAddon());
        } catch {
          // Headless / very old browser — DOM renderer takes over.
        }
      }

      // OSC 52 clipboard write. tmux (with `set -g set-clipboard on`)
      // emits this escape on every drag-select copy: `OSC 52 ; c ;
      // <base64-text> BEL`. Without a handler xterm.js drops it on the
      // floor for security. We accept it and forward to the system
      // clipboard so users get drag-select → release → ready-to-paste
      // without needing Shift overrides or modal "copy mode" toggles.
      // Only the `c` (clipboard) target is honoured; the `p` (primary
      // selection) variant is X11-specific and not useful in a browser.
      term.parser.registerOscHandler(52, (data) => {
        const sep = data.indexOf(';');
        if (sep < 0) return false;
        const targets = data.substring(0, sep);
        const payload = data.substring(sep + 1);
        // Empty `targets` means "default = clipboard"; otherwise we
        // accept any string that includes `c` (clipboard target).
        if (targets !== '' && !targets.includes('c')) return false;
        let text: string;
        try {
          // Two-step decode: atob gives back a Latin-1 "binary string"
          // where each JS char carries one byte. For multi-byte UTF-8
          // (e.g. CJK) we have to re-interpret those bytes as UTF-8 or
          // the clipboard ends up holding mojibake.
          const binary = atob(payload);
          const bytes = new Uint8Array(binary.length);
          for (let i = 0; i < binary.length; i++) {
            bytes[i] = binary.charCodeAt(i);
          }
          text = new TextDecoder('utf-8').decode(bytes);
        } catch {
          return false;
        }
        // navigator.clipboard.writeText is async + needs a "transient
        // user activation" window; mouse-up is one, and OSC 52 arrives
        // microseconds later so the activation is still live. Failures
        // (e.g. http page on a non-localhost host) are silent — we
        // don't want a toast or console spam on every selection.
        navigator.clipboard.writeText(text).catch(() => {});
        return true;
      });

      const id = crypto.randomUUID();

      const ws = new WireSocket({
        onMessage: (msg) => handleTabMsg(id, agent, workspace, tool, msg),
        onBinary: (data) => {
          const tab = tabsRef.current.find((t) => t.id === id);
          tab?.term.write(data);
        },
        onClose: (_code, _reason) => {
          // WS dropped — close the tab so the user doesn't have to
          // click ✕ on a dead session. Refresh the sidebar so the
          // workspace's status dot tracks reality.
          if (ctrlAgentRef.current === agent) {
            ctrlWsRef.current?.send({ type: 'list_workspaces' });
          }
          closeTabRef.current(id);
        },
        onError: () => {
          closeTabRef.current(id);
        },
      });

      // Wire terminal input → WS
      term.onData((data) => {
        const tab = tabsRef.current.find((t) => t.id === id);
        if (tab?.ws.connected) {
          tab.ws.sendBinary(new TextEncoder().encode(data));
        }
      });

      const newTab: Tab = {
        id,
        agent,
        workspace,
        tool,
        status: 'connecting',
        ws,
        term,
        fitAddon,
        opened: false,
      };

      // Cmd+C bridge: when we believe tmux is sitting in copy-mode
      // with an active selection (set by the mouse listeners in
      // attachContainer), pressing Cmd+C sends 'y' to the PTY. tmux's
      // conf binds 'y' to copy-pipe-and-cancel with an OSC 52 emit,
      // which our parser handler then writes to the system clipboard.
      // When the tracking flag is false we let xterm.js's own
      // Cmd+C handling run, so plain text inside an xterm-native
      // selection (rare in this setup) still copies.
      term.attachCustomKeyEventHandler((ev) => {
        if (ev.type !== 'keydown') return true;
        const isCmdC =
          ev.metaKey &&
          !ev.ctrlKey &&
          !ev.shiftKey &&
          !ev.altKey &&
          ev.key.toLowerCase() === 'c';
        if (!isCmdC) return true;
        if (copyModeRef.current.get(id) !== true) return true;
        const me = tabsRef.current.find((t) => t.id === id);
        if (me?.ws.connected) {
          me.ws.sendBinary(new TextEncoder().encode('y'));
        }
        copyModeRef.current.set(id, false);
        return false;
      });

      dispatchTabs({ type: 'ADD', tab: newTab });
      setActiveTabId(id);
      ws.connect();
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  // ── Split pane ────────────────────────────────────────────────────────────

  const handleSplit = useCallback(
    (tabId: string, tool: string, direction: SplitDirection) => {
      const tab = tabsRef.current.find((t) => t.id === tabId);
      if (!tab?.ws.connected) return;
      const args = (KNOWN_TOOLS as readonly string[]).includes(tool)
        ? preferencesRef.current.toolArgs[tool as Tool]
        : [];
      tab.ws.send({
        type: 'split_pane',
        tool,
        direction,
        ...(args.length > 0 ? { args } : {}),
      });
    },
    [],
  );

  const handleChangeLayout = useCallback((tabId: string, layout: PaneLayout) => {
    const tab = tabsRef.current.find((t) => t.id === tabId);
    if (!tab?.ws.connected) return;
    tab.ws.send({ type: 'change_layout', layout });
  }, []);

  // ── PTY WS message handler (per tab) ──────────────────────────────────────

  function handleTabMsg(
    tabId: string,
    agent: string,
    workspace: string,
    tool: string | undefined,
    msg: HubMsg,
  ) {
    switch (msg.type) {
      case 'welcome': {
        // v1.13: skip select_agent. open_session carries the
        // workspace's bound `agent` directly so the hub routes
        // without an extra round-trip.
        dispatchTabs({ type: 'UPDATE', id: tabId, patch: { status: 'opening' } });
        const tab = tabsRef.current.find((t) => t.id === tabId);
        if (!tab) break;
        let cols = 80;
        let rows = 24;
        if (containersRef.current.has(tab.id)) {
          try {
            tab.fitAddon.fit();
            cols = tab.term.cols;
            rows = tab.term.rows;
          } catch {
            // container not yet measured, fall back to defaults
          }
        }
        // Per-user default args, looked up by tool. When the user
        // opened the workspace without an explicit tool we fall back
        // to webterm's own DEFAULT_TOOL so their args still apply —
        // matching the user-visible "click Open == start claude"
        // expectation. If the agent's configured default happens to
        // be a different tool, the args ride along anyway; explicit
        // "Open with X" remains the unambiguous path.
        const effectiveTool: Tool =
          tool && (KNOWN_TOOLS as readonly string[]).includes(tool)
            ? (tool as Tool)
            : DEFAULT_TOOL;
        const args = preferencesRef.current.toolArgs[effectiveTool];
        const openMsg: Parameters<typeof tab.ws.send>[0] = {
          type: 'open_session',
          workspace,
          agent,
          cols,
          rows,
          ...(tool ? { tool } : {}),
          ...(args.length > 0 ? { claude_args: args } : {}),
        };
        tab.ws.send(openMsg);
        break;
      }
      case 'session_opened': {
        dispatchTabs({ type: 'UPDATE', id: tabId, patch: { status: 'live' } });
        // Do a proper fit + resize now that session is open
        setTimeout(() => {
          const tab = tabsRef.current.find((t) => t.id === tabId);
          if (!tab || !containersRef.current.has(tab.id)) return;
          try {
            tab.fitAddon.fit();
            tab.ws.send({ type: 'resize', cols: tab.term.cols, rows: tab.term.rows });
          } catch {
            // ignore
          }
          if (tab.id === activeTabIdRef.current) {
            tab.term.focus();
          }
          // Refresh the cross-agent list so the new "active" badge
          // shows up on the row that just opened.
          ctrlWsRef.current?.send({ type: 'list_workspaces' });
        }, 50);
        break;
      }
      case 'session_error':
        // SessionError is non-fatal by protocol contract (see
        // crates/hub/src/pty_proto.rs). Surface the message as a
        // toast and leave the tab + underlying claude session intact —
        // closing the tab here would discard a live conversation just
        // because Split with codex (or similar) failed.
        addToast(msg.message || 'Session error');
        ctrlWsRef.current?.send({ type: 'list_workspaces' });
        break;
      case 'session_closed':
        // claude exited (/exit, Ctrl+C, crash) — collapse the tab so
        // the user doesn't have to click ✕, and pull a fresh
        // workspace list so the sidebar dot drops from green
        // immediately. v1.13's list_workspaces is cross-agent, so
        // there's no agent-scoped guard to keep here.
        ctrlWsRef.current?.send({ type: 'list_workspaces' });
        closeTabRef.current(tabId);
        break;
      default:
        break;
    }
  }

  // Need a ref to activeTabId inside the session_opened timeout callback
  const activeTabIdRef = useRef<string | null>(null);
  activeTabIdRef.current = activeTabId;

  // ── Close tab ─────────────────────────────────────────────────────────────

  const closeTab = useCallback(
    (id: string) => {
      const all = tabsRef.current;
      const tab = all.find((t) => t.id === id);
      if (tab) {
        tab.ws.close();
        tab.term.dispose();
      }
      dispatchTabs({ type: 'REMOVE', id });

      // Pick next active tab + land the cursor in its xterm. Without
      // the focus deferral, ctrl-C / /exit collapses the current tab,
      // we slide one over, and the user has to click the terminal
      // again before they can type.
      setActiveTabId((prev) => {
        if (prev !== id) return prev;
        const remaining = all.filter((t) => t.id !== id);
        if (remaining.length === 0) return null;
        const idx = all.findIndex((t) => t.id === id);
        const nextId = remaining[Math.min(idx, remaining.length - 1)].id;
        requestAnimationFrame(() => {
          const next = tabsRef.current.find((t) => t.id === nextId);
          if (!next || !containersRef.current.has(next.id)) return;
          try {
            next.fitAddon.fit();
            if (next.ws.connected) {
              next.ws.send({
                type: 'resize',
                cols: next.term.cols,
                rows: next.term.rows,
              });
            }
          } catch {
            // ignore
          }
          next.term.focus();
        });
        return nextId;
      });
    },
    [],
  );

  // openTab's callbacks are created before closeTab in source order
  // but reference it; route through a ref so the call site doesn't
  // close over an undefined identifier on first render.
  const closeTabRef = useRef<(id: string) => void>(() => {});
  closeTabRef.current = closeTab;

  // ── Switch active tab ─────────────────────────────────────────────────────

  const selectTab = useCallback((id: string) => {
    setActiveTabId(id);
    // After state flush, fit + focus the terminal
    requestAnimationFrame(() => {
      const tab = tabsRef.current.find((t) => t.id === id);
      if (!tab || !containersRef.current.has(tab.id)) return;
      try {
        tab.fitAddon.fit();
        if (tab.ws.connected) {
          tab.ws.send({ type: 'resize', cols: tab.term.cols, rows: tab.term.rows });
        }
      } catch {
        // ignore
      }
      tab.term.focus();
    });
  }, []);

  // ── Container ref callbacks (attach xterm after DOM mount) ────────────────

  const attachContainer = useCallback(
    (tabId: string, el: HTMLDivElement | null) => {
      // Stash / clear the DOM node in a ref (not React state) so this
      // callback never triggers a re-render — an inline `ref={(el) =>
      // attachContainer(id, el)}` is a fresh closure every render,
      // which React treats as a ref change. If the callback caused a
      // setState we'd infinite-loop.
      if (!el) {
        containersRef.current.delete(tabId);
        return;
      }
      containersRef.current.set(tabId, el);
      const tab = tabsRef.current.find((t) => t.id === tabId);
      if (!tab || tab.opened) return;
      try {
        tab.term.open(el);
        tab.fitAddon.fit();
        tab.opened = true;
      } catch {
        // StrictMode double-mount — already opened
      }

      // Track whether tmux is currently sitting in copy-mode with a
      // live selection. Set after a drag-with-movement mouseup;
      // cleared on any plain mousedown (which also fires tmux's
      // MouseDown-in-copy-mode → cancel binding) or after a Cmd+C
      // bridges through to tmux. Capture phase so we see the events
      // before xterm.js forwards them upstream. The state lives in
      // copyModeRef (Map<tabId, bool>), not on the Tab object,
      // because the tabs reducer rebuilds Tab references on every
      // UPDATE and would otherwise discard our mutation.
      let downX = 0;
      let downY = 0;
      let movedDuringDrag = false;

      el.addEventListener(
        'mousedown',
        (ev) => {
          downX = ev.clientX;
          downY = ev.clientY;
          movedDuringDrag = false;
          copyModeRef.current.set(tabId, false);
        },
        true,
      );
      el.addEventListener(
        'mousemove',
        (ev) => {
          if (!(ev.buttons & 1)) return;
          if (Math.abs(ev.clientX - downX) > 4 || Math.abs(ev.clientY - downY) > 4) {
            movedDuringDrag = true;
          }
        },
        true,
      );
      el.addEventListener(
        'mouseup',
        () => {
          if (movedDuringDrag) copyModeRef.current.set(tabId, true);
        },
        true,
      );
    },
    [],
  );

  // ── ResizeObserver: fit active terminal on resize ─────────────────────────

  useEffect(() => {
    if (!activeTabId) return;
    const tab = tabsRef.current.find((t) => t.id === activeTabId);
    const el = containersRef.current.get(activeTabId);
    if (!tab || !el) return;

    let timer: ReturnType<typeof setTimeout> | null = null;
    const ro = new ResizeObserver(() => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        try {
          tab.fitAddon.fit();
          if (tab.ws.connected) {
            tab.ws.send({ type: 'resize', cols: tab.term.cols, rows: tab.term.rows });
          }
        } catch {
          // ignore
        }
      }, 150);
    });
    ro.observe(el);
    return () => {
      ro.disconnect();
      if (timer) clearTimeout(timer);
    };
  }, [activeTabId]);

  // ── Theme change: update all terminals ───────────────────────────────────

  function handleThemeChange(t: Theme) {
    const isDark = effectiveTheme(t) === 'dark';
    tabsRef.current.forEach((tab) => {
      tab.term.options.theme = xtermTheme(isDark);
    });
  }

  // ── Logout ────────────────────────────────────────────────────────────────

  function handleLogout() {
    apiClient.logout().finally(() => navigate('/login', { replace: true }));
  }

  // ── Computed: set of open tab keys ───────────────────────────────────────

  const openTabKeys = new Set(tabs.map((t) => tabKey(t.agent, t.workspace)));
  const activeTab = tabs.find((t) => t.id === activeTabId) ?? null;
  const activeTabKey = activeTab ? tabKey(activeTab.agent, activeTab.workspace) : null;

  // ── Render ────────────────────────────────────────────────────────────────

  if (authLoading) {
    return (
      <div className="h-full flex items-center justify-center text-zinc-500 text-sm">
        Loading...
      </div>
    );
  }

  void ctrlReady; // used to suppress unused-var lint; ctrlReady drives UI indirectly via agentsLoading

  return (
    <div className="h-full flex overflow-hidden bg-white dark:bg-zinc-950">
      {/* Left sidebar */}
      <Sidebar
        account={account}
        agents={agents}
        agentsLoading={agentsLoading}
        workspaces={workspaces}
        openTabKeys={openTabKeys}
        activeTabKey={activeTabKey}
        onOpenWorkspace={openTab}
        onCreateWorkspace={handleCreateWorkspace}
        onResetWorkspace={handleResetWorkspace}
        onDeleteWorkspace={handleDeleteWorkspace}
        onSettings={() => setShowSettings(true)}
        onLogout={handleLogout}
      />

      {/* Right: tab bar + terminal area */}
      <div className="flex-1 flex flex-col overflow-hidden">
        {/* Tab bar */}
        <TabBar
          tabs={tabs}
          activeTabId={activeTabId}
          onSelect={selectTab}
          onClose={closeTab}
          onSplit={handleSplit}
          onChangeLayout={handleChangeLayout}
        />

        {/* Terminal containers — all rendered, visibility toggled via class */}
        <div className="flex-1 relative overflow-hidden bg-white dark:bg-zinc-950">
          {tabs.length === 0 && (
            <div className="absolute inset-0 flex items-center justify-center text-sm text-zinc-400 dark:text-zinc-600 select-none">
              Open a workspace from the sidebar to start
            </div>
          )}

          {tabs.map((tab) => (
            <div
              key={tab.id}
              ref={(el) => attachContainer(tab.id, el)}
              className={`absolute inset-0 ${tab.id === activeTabId ? 'block' : 'hidden'}`}
            >
              {/* Status overlays */}
              {(tab.status === 'connecting' || tab.status === 'opening') && (
                <div className="absolute inset-0 flex items-center justify-center bg-white/80 dark:bg-zinc-950/80 z-10 pointer-events-none">
                  <span className="text-sm text-zinc-500 dark:text-zinc-400">
                    {tab.status === 'connecting' ? 'Connecting...' : 'Opening session...'}
                  </span>
                </div>
              )}
              {(tab.status === 'closed' || tab.status === 'error') && (
                <div className="absolute inset-0 flex flex-col items-center justify-center gap-4 z-10 bg-white/90 dark:bg-zinc-950/90">
                  <div className="rounded-lg bg-red-50 dark:bg-red-950 border border-red-200 dark:border-red-900 px-6 py-4 text-sm text-red-700 dark:text-red-400 max-w-md text-center">
                    {tab.errorMsg ?? 'Session ended'}
                  </div>
                  <button
                    onClick={() => closeTab(tab.id)}
                    className="text-sm px-4 py-2 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
                  >
                    Close tab
                  </button>
                </div>
              )}
            </div>
          ))}
        </div>
      </div>

      {/* Settings modal */}
      {showSettings && (
        <SettingsDialog
          onClose={() => setShowSettings(false)}
          onThemeChange={handleThemeChange}
          preferences={preferences}
          onSavePreferences={savePreferences}
        />
      )}

      {/* Transient error toasts (non-fatal SessionError frames) */}
      {toasts.length > 0 && (
        <div className="pointer-events-none fixed bottom-4 right-4 z-50 flex max-w-md flex-col gap-2">
          {toasts.map((t) => (
            <div
              key={t.id}
              className="pointer-events-auto flex items-start gap-2 rounded-md border border-red-200 dark:border-red-900 bg-red-50 dark:bg-red-950 px-3 py-2 text-xs font-mono text-red-700 dark:text-red-300 shadow-lg"
              role="alert"
            >
              <span className="flex-1 break-words">{t.message}</span>
              <button
                type="button"
                onClick={() => dismissToast(t.id)}
                className="shrink-0 rounded p-0.5 opacity-60 hover:opacity-100 hover:bg-red-100 dark:hover:bg-red-900"
                aria-label="Dismiss"
              >
                <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
                  <path d="M2 2L8 8M8 2L2 8" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
                </svg>
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
