// Full-screen terminal session page.
// Opens the WireSocket, sends open_session after xterm fit, streams PTY I/O.

import { useEffect, useRef, useCallback, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import Term, { TermHandle } from '@/components/Term';
import { WireSocket, HubMsg } from '@/lib/wire';
import { apiClient } from '@/lib/api';
import { setStoredTheme, getStoredTheme, Theme, effectiveTheme } from '@/lib/theme';
import Logo from '@/components/Logo';

type SessionInfo = {
  agent: string;
  workspace: string;
  cwd: string;
};

export default function Session() {
  const navigate = useNavigate();
  const termRef = useRef<TermHandle | null>(null);
  const wsRef = useRef<WireSocket | null>(null);
  const resizeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);

  const [sessionInfo, setSessionInfo] = useState<SessionInfo | null>(null);
  const [account, setAccount] = useState('');
  const [loading, setLoading] = useState(true);
  const [errorMsg, setErrorMsg] = useState('');
  const [showSettings, setShowSettings] = useState(false);
  const [theme, setTheme] = useState<Theme>(getStoredTheme);

  // ── Back to picker ─────────────────────────────────────────────────────────

  const handleBack = useCallback(() => {
    if (wsRef.current) {
      wsRef.current.close();
      wsRef.current = null;
    }
    sessionStorage.removeItem('cc_session');
    navigate('/');
  }, [navigate]);

  // ── WireSocket messages ────────────────────────────────────────────────────

  const handleHubMsg = useCallback(
    (msg: HubMsg) => {
      switch (msg.type) {
        case 'session_opened': {
          const info: SessionInfo = {
            agent: msg.agent,
            workspace: msg.workspace,
            cwd: msg.cwd,
          };
          setSessionInfo(info);
          sessionStorage.setItem('cc_session', JSON.stringify(info));
          setLoading(false);
          // Correct the cols/rows now that xterm is mounted and fit
          setTimeout(() => {
            const dims = termRef.current?.fit();
            if (dims && wsRef.current) {
              wsRef.current.send({ type: 'resize', cols: dims.cols, rows: dims.rows });
            }
            termRef.current?.focus();
          }, 50);
          break;
        }
        case 'session_error':
          setErrorMsg(msg.message);
          setLoading(false);
          break;
        case 'session_closed':
          setErrorMsg(msg.reason ?? 'Session closed');
          setLoading(false);
          break;
        default:
          break;
      }
    },
    [],
  );

  // ── Mount: connect WS + open session ──────────────────────────────────────

  useEffect(() => {
    // Get account for display
    apiClient.me().then((me) => setAccount(me.account)).catch(() => {});

    const ws = new WireSocket({
      onMessage: handleHubMsg,
      onBinary: (data) => {
        termRef.current?.write(data);
      },
      onClose: (_code, reason) => {
        setLoading(false);
        if (reason) setErrorMsg(`Disconnected: ${reason}`);
        wsRef.current = null;
      },
      onError: () => {
        setErrorMsg('WebSocket connection failed');
        setLoading(false);
      },
    });

    // Restore state from sessionStorage so we know what to open
    const raw = sessionStorage.getItem('cc_session');
    const stored = raw ? (JSON.parse(raw) as Partial<SessionInfo>) : null;

    const onceWelcome = (msg: HubMsg) => {
      if (msg.type === 'welcome') {
        // Need workspace. Read from sessionStorage if already set (re-mount),
        // or wait for Picker to have set it. Picker always sets it before navigating.
        const workspace = stored?.workspace ?? '';
        if (!workspace) {
          setErrorMsg('No workspace selected.');
          setLoading(false);
          return;
        }
        // Restore agent selection first if needed, then open session
        if (stored?.agent) {
          ws.send({ type: 'select_agent', agent: stored.agent });
        }
        // open_session with default 80x24; Session will resize after fit
        ws.send({ type: 'open_session', workspace, cols: 80, rows: 24 });
      }
      handleHubMsg(msg);
    };

    // Patch handler for welcome
    const wsAny = ws as unknown as { handlers: { onMessage: (m: HubMsg) => void } };
    wsAny.handlers.onMessage = onceWelcome;

    ws.connect();
    wsRef.current = ws;

    return () => {
      ws.close();
      wsRef.current = null;
    };
  // handleHubMsg is stable (useCallback with no deps)
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ── ResizeObserver → fit → resize ─────────────────────────────────────────

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    const ro = new ResizeObserver(() => {
      if (resizeTimer.current) clearTimeout(resizeTimer.current);
      resizeTimer.current = setTimeout(() => {
        const dims = termRef.current?.fit();
        if (dims && wsRef.current?.connected) {
          wsRef.current.send({ type: 'resize', cols: dims.cols, rows: dims.rows });
        }
      }, 200);
    });

    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // ── xterm data → WS binary ────────────────────────────────────────────────

  const handleData = useCallback((data: string) => {
    wsRef.current?.sendBinary(new TextEncoder().encode(data));
  }, []);

  // ── Theme ─────────────────────────────────────────────────────────────────

  function handleTheme(t: Theme) {
    setTheme(t);
    setStoredTheme(t);
    termRef.current?.setDark(effectiveTheme(t) === 'dark');
  }

  // ── Logout ────────────────────────────────────────────────────────────────

  function handleLogout() {
    apiClient.logout().finally(() => navigate('/login', { replace: true }));
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  return (
    <div className="h-full flex flex-col bg-white dark:bg-zinc-950">
      {/* Thin top bar */}
      <div className="shrink-0 border-b border-zinc-200 dark:border-zinc-800 bg-white dark:bg-zinc-900 px-3 py-1.5 flex items-center gap-2">
        <button
          onClick={handleBack}
          className="text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100 transition-colors mr-1"
        >
          &larr; menu
        </button>
        <Logo size={16} className="text-zinc-500 shrink-0" />
        <span className="text-xs text-zinc-500 dark:text-zinc-400 flex-1 truncate">
          {sessionInfo
            ? `${sessionInfo.agent} · ${sessionInfo.workspace} · ${account}`
            : account || 'connecting...'}
        </span>
        <button
          onClick={() => setShowSettings(true)}
          className="text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100 px-1.5 transition-colors"
        >
          settings
        </button>
        <button
          onClick={handleLogout}
          className="text-xs text-zinc-400 hover:text-red-500 dark:hover:text-red-400 px-1.5 transition-colors"
        >
          logout
        </button>
      </div>

      {/* Terminal area */}
      <div ref={containerRef} className="flex-1 relative overflow-hidden">
        {/* Loading overlay */}
        {loading && (
          <div className="absolute inset-0 flex items-center justify-center bg-white/80 dark:bg-zinc-950/80 z-10">
            <span className="text-sm text-zinc-500 dark:text-zinc-400">Opening session...</span>
          </div>
        )}

        {/* Error overlay */}
        {!loading && errorMsg && (
          <div className="absolute inset-0 flex flex-col items-center justify-center gap-4 z-10">
            <div className="rounded-lg bg-red-50 dark:bg-red-950 border border-red-200 dark:border-red-900 px-6 py-4 text-sm text-red-700 dark:text-red-400 max-w-md text-center">
              {errorMsg}
            </div>
            <button
              onClick={handleBack}
              className="text-sm px-4 py-2 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
            >
              Back to menu
            </button>
          </div>
        )}

        <Term ref={termRef} onData={handleData} />
      </div>

      {/* Settings modal */}
      {showSettings && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40">
          <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-xl p-6 w-full max-w-sm mx-4">
            <h3 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 mb-4">
              Settings
            </h3>
            <div className="mb-4">
              <p className="text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-2 uppercase tracking-wide">
                Theme
              </p>
              <div className="flex gap-2">
                {(['system', 'light', 'dark'] as Theme[]).map((t) => (
                  <button
                    key={t}
                    onClick={() => handleTheme(t)}
                    className={`flex-1 text-sm py-1.5 rounded-lg border transition-colors capitalize ${
                      theme === t
                        ? 'bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 border-transparent'
                        : 'border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800'
                    }`}
                  >
                    {t}
                  </button>
                ))}
              </div>
            </div>
            <div className="flex justify-end">
              <button
                onClick={() => setShowSettings(false)}
                className="text-sm px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
              >
                Close
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
