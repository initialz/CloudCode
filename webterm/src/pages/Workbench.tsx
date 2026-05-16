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
import '@xterm/xterm/css/xterm.css';

import { apiClient } from '@/lib/api';
import {
  WireSocket,
  type AgentItem,
  type HubMsg,
} from '@/lib/wire';
import { effectiveTheme, getStoredTheme, type Theme } from '@/lib/theme';
import { type Tab, tabKey } from '@/lib/tabs';
import Sidebar from '@/components/Sidebar';
import TabBar from '@/components/TabBar';
import SettingsDialog from '@/components/SettingsDialog';
import type { AgentWorkspaceCache } from '@/components/AgentTree';

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
  const [wsCache, setWsCache] = useState<AgentWorkspaceCache>(new Map());

  // Currently "selected" agent on the control connection (needed for create/list)
  const ctrlAgentRef = useRef<string | null>(null);

  // Tabs
  const [tabs, dispatchTabs] = useReducer(tabsReducer, []);
  const tabsRef = useRef<Tab[]>(tabs);
  tabsRef.current = tabs;
  const [activeTabId, setActiveTabId] = useState<string | null>(null);

  // Settings dialog
  const [showSettings, setShowSettings] = useState(false);

  // Refresh timer ref (30s poll)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Container DOM nodes per tab id. Kept outside of React state on
  // purpose — touching them during a ref callback must NOT trigger
  // a re-render, or the inline ref creates an infinite loop.
  const containersRef = useRef<Map<string, HTMLDivElement>>(new Map());

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

  // ── Control WS helpers ─────────────────────────────────────────────────────

  const refreshWorkspaces = useCallback((agent: string) => {
    if (!ctrlWsRef.current?.connected) return;
    // If control ws is already on this agent, just list; otherwise switch first.
    if (ctrlAgentRef.current === agent) {
      ctrlWsRef.current.send({ type: 'list_workspaces' });
    } else {
      ctrlWsRef.current.send({ type: 'select_agent', agent });
    }
  }, []);

  const schedulePoll = useCallback(() => {
    if (pollTimerRef.current) clearTimeout(pollTimerRef.current);
    pollTimerRef.current = setTimeout(() => {
      // Re-fetch all currently expanded agents
      if (ctrlAgentRef.current) {
        refreshWorkspaces(ctrlAgentRef.current);
      }
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
          break;

        case 'agent_list':
          setAgents(msg.items);
          setAgentsLoading(false);
          break;

        case 'agent_selected':
          ctrlAgentRef.current = msg.agent;
          // Mark as loading in cache
          setWsCache((prev) => {
            const next = new Map(prev);
            if (!next.has(msg.agent) || next.get(msg.agent)?.status === 'idle') {
              next.set(msg.agent, { status: 'loading' });
            }
            return next;
          });
          ctrlWsRef.current?.send({ type: 'list_workspaces' });
          break;

        case 'workspace_list':
          if (ctrlAgentRef.current) {
            const agent = ctrlAgentRef.current;
            setWsCache((prev) => {
              const next = new Map(prev);
              next.set(agent, { status: 'loaded', items: msg.items });
              return next;
            });
          }
          break;

        case 'workspace_created':
        case 'workspace_deleted':
        case 'workspace_reset':
          // Refresh the current agent's list
          if (ctrlAgentRef.current) {
            ctrlWsRef.current?.send({ type: 'list_workspaces' });
          }
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

  function handleExpandAgent(agent: string) {
    // Mark as loading if first time
    setWsCache((prev) => {
      const next = new Map(prev);
      if (!next.has(agent)) {
        next.set(agent, { status: 'loading' });
      }
      return next;
    });

    if (!ctrlWsRef.current?.connected) return;

    if (ctrlAgentRef.current === agent) {
      // Already selected — just refresh
      ctrlWsRef.current.send({ type: 'list_workspaces' });
    } else {
      // Switch agent on control ws (triggers agent_selected → list_workspaces)
      ctrlWsRef.current.send({ type: 'select_agent', agent });
    }
  }

  function handleCreateWorkspace(agent: string, name: string) {
    if (!ctrlWsRef.current?.connected) return;
    if (ctrlAgentRef.current !== agent) {
      // Switch then create — create_workspace fires after agent_selected
      // We need to queue the create; simplest: switch and rely on the
      // agent_selected handler, but we don't have a queue mechanism.
      // Instead switch synchronously and immediately send create.
      ctrlWsRef.current.send({ type: 'select_agent', agent });
    }
    ctrlWsRef.current.send({ type: 'create_workspace', name });
  }

  function handleResetWorkspace(agent: string, workspace: string) {
    if (!ctrlWsRef.current?.connected) return;
    if (ctrlAgentRef.current !== agent) {
      ctrlWsRef.current.send({ type: 'select_agent', agent });
    }
    ctrlWsRef.current.send({ type: 'reset_workspace', name: workspace });
  }

  function handleDeleteWorkspace(agent: string, workspace: string) {
    if (!ctrlWsRef.current?.connected) return;
    if (ctrlAgentRef.current !== agent) {
      ctrlWsRef.current.send({ type: 'select_agent', agent });
    }
    ctrlWsRef.current.send({ type: 'delete_workspace', name: workspace });
  }

  // ── Open tab ──────────────────────────────────────────────────────────────

  const openTab = useCallback(
    (agent: string, workspace: string) => {
      // Deduplicate
      const key = tabKey(agent, workspace);
      const existing = tabsRef.current.find(
        (t) => tabKey(t.agent, t.workspace) === key,
      );
      if (existing) {
        setActiveTabId(existing.id);
        return;
      }

      const isDark = effectiveTheme(getStoredTheme()) === 'dark';
      const term = new Terminal({
        cursorBlink: true,
        scrollback: 10000,
        fontFamily: 'ui-monospace, Menlo, Monaco, monospace',
        fontSize: 14,
        theme: xtermTheme(isDark),
      });
      const fitAddon = new FitAddon();
      const linksAddon = new WebLinksAddon();
      term.loadAddon(fitAddon);
      term.loadAddon(linksAddon);

      const id = crypto.randomUUID();

      const ws = new WireSocket({
        onMessage: (msg) => handleTabMsg(id, agent, workspace, msg),
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
        status: 'connecting',
        ws,
        term,
        fitAddon,
        opened: false,
      };

      dispatchTabs({ type: 'ADD', tab: newTab });
      setActiveTabId(id);
      ws.connect();
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  // ── PTY WS message handler (per tab) ──────────────────────────────────────

  function handleTabMsg(
    tabId: string,
    agent: string,
    workspace: string,
    msg: HubMsg,
  ) {
    switch (msg.type) {
      case 'welcome': {
        dispatchTabs({ type: 'UPDATE', id: tabId, patch: { status: 'opening' } });
        // Select agent, then open session
        const tab = tabsRef.current.find((t) => t.id === tabId);
        if (!tab) break;
        tab.ws.send({ type: 'select_agent', agent });
        break;
      }
      case 'agent_selected': {
        // Now open session. Get current fit dimensions.
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
        tab.ws.send({ type: 'open_session', workspace, cols, rows });
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
          // Refresh workspace list to show active dot
          if (ctrlAgentRef.current === agent) {
            ctrlWsRef.current?.send({ type: 'list_workspaces' });
          }
        }, 50);
        break;
      }
      case 'session_error':
      case 'session_closed':
        // claude exited (/exit, Ctrl+C, crash) or the open failed —
        // collapse the tab immediately so the user doesn't have to
        // click ✕. The sidebar's status dot tracks the workspace
        // separately.
        if (ctrlAgentRef.current === agent) {
          ctrlWsRef.current?.send({ type: 'list_workspaces' });
        }
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

      // Pick next active tab
      setActiveTabId((prev) => {
        if (prev !== id) return prev;
        const remaining = all.filter((t) => t.id !== id);
        if (remaining.length === 0) return null;
        const idx = all.findIndex((t) => t.id === id);
        return remaining[Math.min(idx, remaining.length - 1)].id;
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
        cache={wsCache}
        openTabKeys={openTabKeys}
        onExpandAgent={handleExpandAgent}
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
        />
      )}
    </div>
  );
}
