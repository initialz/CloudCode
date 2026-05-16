// Agent + Workspace two-step picker.
// Builds and owns the WireSocket; passes it to Session via location.state.

import {
  useState,
  useEffect,
  useCallback,
  useRef,
} from 'react';
import { useNavigate } from 'react-router-dom';
import Logo from '@/components/Logo';
import { apiClient, ApiError } from '@/lib/api';
import { WireSocket, AgentItem, WorkspaceItem, HubMsg } from '@/lib/wire';
import { setStoredTheme, getStoredTheme, Theme } from '@/lib/theme';

type Step = 'agents' | 'workspaces';

const DEFAULT_COLS = 80;
const DEFAULT_ROWS = 24;

export default function Picker() {
  const navigate = useNavigate();

  // auth
  const [account, setAccount] = useState('');
  const [authLoading, setAuthLoading] = useState(true);

  // ws
  const wsRef = useRef<WireSocket | null>(null);
  const [wsReady, setWsReady] = useState(false);
  const [wsError, setWsError] = useState('');

  // picker state
  const [step, setStep] = useState<Step>('agents');
  const [agents, setAgents] = useState<AgentItem[]>([]);
  const [selectedAgent, setSelectedAgent] = useState('');
  const [workspaces, setWorkspaces] = useState<WorkspaceItem[]>([]);

  // feedback
  const [banner, setBanner] = useState('');
  const [actionLoading, setActionLoading] = useState('');

  // settings modal
  const [showSettings, setShowSettings] = useState(false);
  const [theme, setTheme] = useState<Theme>(getStoredTheme);

  // create workspace
  const [showCreate, setShowCreate] = useState(false);
  const [createName, setCreateName] = useState('');

  // confirm dialog
  const [confirm, setConfirm] = useState<{ action: string; name: string } | null>(null);

  // ── Auth check ─────────────────────────────────────────────────────────────

  useEffect(() => {
    apiClient
      .me()
      .then((me) => {
        setAccount(me.account);
        setAuthLoading(false);
      })
      .catch((err: ApiError) => {
        if (err.status === 401) navigate('/login', { replace: true });
        else setBanner(err.message);
        setAuthLoading(false);
      });
  }, [navigate]);

  // ── WireSocket lifecycle ───────────────────────────────────────────────────

  const handleHubMsg = useCallback(
    (msg: HubMsg) => {
      switch (msg.type) {
        case 'agent_list':
          setAgents(msg.items);
          break;
        case 'agent_selected':
          setSelectedAgent(msg.agent);
          setStep('workspaces');
          // request workspace list right away
          wsRef.current?.send({ type: 'list_workspaces' });
          break;
        case 'workspace_list':
          setWorkspaces(msg.items);
          break;
        case 'workspace_created':
          wsRef.current?.send({ type: 'list_workspaces' });
          setActionLoading('');
          setShowCreate(false);
          setCreateName('');
          break;
        case 'workspace_deleted':
        case 'workspace_reset':
          wsRef.current?.send({ type: 'list_workspaces' });
          setActionLoading('');
          break;
        case 'session_opened':
          // Navigate to session, pass ws ref via sessionStorage key so we
          // don't lose the open socket. We store the chosen names.
          sessionStorage.setItem(
            'cc_session',
            JSON.stringify({
              agent: msg.agent,
              workspace: msg.workspace,
              cwd: msg.cwd,
            }),
          );
          navigate('/session');
          break;
        case 'session_error':
          setBanner(`Session error: ${msg.message}`);
          setActionLoading('');
          break;
        case 'rejected':
          setBanner(`Rejected: ${msg.reason}`);
          break;
        default:
          break;
      }
    },
    [navigate],
  );

  useEffect(() => {
    if (authLoading) return;

    const ws = new WireSocket({
      onMessage: handleHubMsg,
      onBinary: () => {},
      onClose: (_code, reason) => {
        setWsReady(false);
        if (reason) setBanner(`Connection closed: ${reason}`);
      },
      onError: () => {
        setWsError('WebSocket connection failed');
      },
    });

    ws.connect();
    wsRef.current = ws;

    // Wait for welcome, then list agents
    const origOnMessage = handleHubMsg;
    const onceWelcome = (msg: HubMsg) => {
      if (msg.type === 'welcome') {
        setWsReady(true);
        ws.send({ type: 'list_agents' });
      }
      origOnMessage(msg);
    };

    // Patch handlers temporarily for welcome
    const wsAny = ws as unknown as { handlers: { onMessage: (m: HubMsg) => void } };
    wsAny.handlers.onMessage = onceWelcome;

    return () => {
      ws.close();
      wsRef.current = null;
    };
  }, [authLoading, handleHubMsg]);

  // ── Workspace badge ────────────────────────────────────────────────────────

  function wsBadge(w: WorkspaceItem) {
    if (w.tmux_alive && w.has_client) return { label: 'active', cls: 'bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-300' };
    if (w.tmux_alive) return { label: 'saved', cls: 'bg-yellow-100 text-yellow-800 dark:bg-yellow-900 dark:text-yellow-300' };
    return { label: 'fresh', cls: 'bg-zinc-100 text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400' };
  }

  // ── Actions ────────────────────────────────────────────────────────────────

  function selectAgent(name: string) {
    wsRef.current?.send({ type: 'select_agent', agent: name });
  }

  function openWorkspace(name: string) {
    setBanner('');
    setActionLoading(name);
    wsRef.current?.send({
      type: 'open_session',
      workspace: name,
      cols: DEFAULT_COLS,
      rows: DEFAULT_ROWS,
    });
  }

  function createWorkspace() {
    const name = createName.trim();
    if (!name) return;
    setActionLoading('create');
    wsRef.current?.send({ type: 'create_workspace', name });
  }

  function confirmAction(action: string, name: string) {
    setConfirm({ action, name });
  }

  function executeConfirmed() {
    if (!confirm) return;
    const { action, name } = confirm;
    setActionLoading(action + ':' + name);
    setConfirm(null);
    if (action === 'delete') wsRef.current?.send({ type: 'delete_workspace', name });
    if (action === 'reset') wsRef.current?.send({ type: 'reset_workspace', name });
  }

  function handleLogout() {
    apiClient.logout().finally(() => navigate('/login', { replace: true }));
  }

  function handleTheme(t: Theme) {
    setTheme(t);
    setStoredTheme(t);
  }

  // ── Render helpers ─────────────────────────────────────────────────────────

  if (authLoading) {
    return (
      <div className="min-h-full flex items-center justify-center text-zinc-500 text-sm">
        Loading...
      </div>
    );
  }

  return (
    <div className="min-h-full flex flex-col">
      {/* Top nav */}
      <nav className="border-b border-zinc-200 dark:border-zinc-800 bg-white dark:bg-zinc-900 px-4 py-2.5 flex items-center gap-3">
        <Logo size={24} className="text-zinc-900 dark:text-zinc-100 shrink-0" />
        <span className="text-sm font-medium text-zinc-900 dark:text-zinc-100 flex-1">
          cloudcode
        </span>
        <span className="text-xs text-zinc-500 dark:text-zinc-400">{account}</span>
        <button
          onClick={() => setShowSettings(true)}
          className="text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100 px-2 py-1 rounded transition-colors"
        >
          Settings
        </button>
        <button
          onClick={handleLogout}
          className="text-xs text-zinc-500 hover:text-red-600 dark:hover:text-red-400 px-2 py-1 rounded transition-colors"
        >
          Logout
        </button>
      </nav>

      {/* Main */}
      <div className="flex-1 max-w-2xl mx-auto w-full px-4 py-8">
        {/* Banner */}
        {(banner || wsError) && (
          <div className="mb-4 rounded-lg bg-red-50 dark:bg-red-950 border border-red-200 dark:border-red-900 px-4 py-3 text-sm text-red-700 dark:text-red-400 flex items-start gap-2">
            <span className="flex-1">{banner || wsError}</span>
            <button
              onClick={() => { setBanner(''); setWsError(''); }}
              className="text-red-400 hover:text-red-600 shrink-0"
            >
              x
            </button>
          </div>
        )}

        {!wsReady && !wsError && (
          <p className="text-sm text-zinc-500 dark:text-zinc-400 mb-4">Connecting...</p>
        )}

        {/* Agent picker */}
        {step === 'agents' && (
          <section>
            <h2 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 mb-4">
              Select agent
            </h2>
            {agents.length === 0 && wsReady && (
              <p className="text-sm text-zinc-500">No agents available.</p>
            )}
            <div className="grid gap-2">
              {agents.map((a) => (
                <button
                  key={a.name}
                  onClick={() => selectAgent(a.name)}
                  className="flex items-center gap-3 w-full rounded-lg border border-zinc-200 dark:border-zinc-800 bg-white dark:bg-zinc-900 px-4 py-3 text-left hover:border-zinc-400 dark:hover:border-zinc-600 hover:shadow-sm transition-all"
                >
                  <span className="text-sm font-medium text-zinc-900 dark:text-zinc-100 flex-1">
                    {a.name}
                  </span>
                  {a.current && (
                    <span className="text-xs bg-blue-100 text-blue-700 dark:bg-blue-900 dark:text-blue-300 rounded px-1.5 py-0.5">
                      current
                    </span>
                  )}
                </button>
              ))}
            </div>
          </section>
        )}

        {/* Workspace picker */}
        {step === 'workspaces' && (
          <section>
            <div className="flex items-center gap-3 mb-4">
              <button
                onClick={() => {
                  setStep('agents');
                  wsRef.current?.send({ type: 'list_agents' });
                }}
                className="text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100 transition-colors"
              >
                &larr; Agents
              </button>
              <h2 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 flex-1">
                {selectedAgent} &mdash; workspaces
              </h2>
              <button
                onClick={() => setShowCreate(true)}
                className="text-xs bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 rounded px-2.5 py-1 hover:bg-zinc-700 dark:hover:bg-zinc-300 transition-colors"
              >
                + Create
              </button>
            </div>

            {workspaces.length === 0 && wsReady && (
              <p className="text-sm text-zinc-500 dark:text-zinc-400">
                No workspaces yet. Create one to get started.
              </p>
            )}

            <div className="grid gap-2">
              {workspaces.map((w) => {
                const badge = wsBadge(w);
                const isOpening = actionLoading === w.name;
                return (
                  <div
                    key={w.name}
                    className="flex items-center gap-3 rounded-lg border border-zinc-200 dark:border-zinc-800 bg-white dark:bg-zinc-900 px-4 py-3"
                  >
                    <button
                      onClick={() => openWorkspace(w.name)}
                      disabled={!!actionLoading}
                      className="flex-1 text-left"
                    >
                      <span className="text-sm font-medium text-zinc-900 dark:text-zinc-100">
                        {isOpening ? 'Opening...' : w.name}
                      </span>
                    </button>
                    <span className={`text-xs rounded px-1.5 py-0.5 font-medium ${badge.cls}`}>
                      {badge.label}
                    </span>
                    <button
                      onClick={() => confirmAction('reset', w.name)}
                      disabled={!!actionLoading}
                      className="text-xs text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-300 px-1.5 py-0.5 transition-colors"
                      title="Reset workspace"
                    >
                      reset
                    </button>
                    <button
                      onClick={() => confirmAction('delete', w.name)}
                      disabled={!!actionLoading}
                      className="text-xs text-zinc-400 hover:text-red-600 dark:hover:text-red-400 px-1.5 py-0.5 transition-colors"
                      title="Delete workspace"
                    >
                      delete
                    </button>
                  </div>
                );
              })}
            </div>
          </section>
        )}
      </div>

      {/* Create workspace modal */}
      {showCreate && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40">
          <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-xl p-6 w-full max-w-sm mx-4">
            <h3 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 mb-4">
              Create workspace
            </h3>
            <input
              type="text"
              placeholder="workspace name"
              value={createName}
              onChange={(e) => setCreateName(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && createWorkspace()}
              autoFocus
              className="w-full rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-800 px-3 py-2 text-sm text-zinc-900 dark:text-zinc-100 placeholder-zinc-400 focus:outline-none focus:ring-2 focus:ring-zinc-500 mb-4"
            />
            <div className="flex gap-2 justify-end">
              <button
                onClick={() => { setShowCreate(false); setCreateName(''); }}
                className="text-sm px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
              >
                Cancel
              </button>
              <button
                onClick={createWorkspace}
                disabled={!createName.trim()}
                className="text-sm px-3 py-1.5 rounded-lg bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:bg-zinc-700 dark:hover:bg-zinc-300 disabled:opacity-50 transition-colors"
              >
                Create
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Confirm dialog */}
      {confirm && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40">
          <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-xl p-6 w-full max-w-sm mx-4">
            <h3 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 mb-2">
              {confirm.action === 'delete' ? 'Delete' : 'Reset'} workspace?
            </h3>
            <p className="text-sm text-zinc-600 dark:text-zinc-400 mb-4">
              {confirm.action === 'delete'
                ? `This will permanently delete "${confirm.name}".`
                : `This will reset "${confirm.name}" to a fresh state.`}
            </p>
            <div className="flex gap-2 justify-end">
              <button
                onClick={() => setConfirm(null)}
                className="text-sm px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
              >
                Cancel
              </button>
              <button
                onClick={executeConfirmed}
                className={`text-sm px-3 py-1.5 rounded-lg text-white transition-colors ${
                  confirm.action === 'delete'
                    ? 'bg-red-600 hover:bg-red-700'
                    : 'bg-zinc-900 dark:bg-zinc-100 dark:text-zinc-900 hover:bg-zinc-700 dark:hover:bg-zinc-300'
                }`}
              >
                {confirm.action === 'delete' ? 'Delete' : 'Reset'}
              </button>
            </div>
          </div>
        </div>
      )}

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
