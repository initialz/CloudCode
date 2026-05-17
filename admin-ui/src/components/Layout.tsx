import { useEffect, useRef, useState } from 'react';
import { NavLink, Outlet, useNavigate } from 'react-router-dom';
import { apiClient } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import { compareSemver } from '@/lib/version';
import { SettingsModal } from './SettingsModal';
import { Logo } from './Logo';

type UpdateState =
  | { kind: 'idle' }
  | { kind: 'available'; latest: string }
  | { kind: 'updating' }
  | { kind: 'waiting' } // hub restarting; polling /me until it comes back
  | { kind: 'failed'; message: string };

export function Layout() {
  const { setOut } = useAuth();
  const nav = useNavigate();
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [hubVersion, setHubVersion] = useState<string | null>(null);
  const [update, setUpdate] = useState<UpdateState>({ kind: 'idle' });
  const [dot, setDot] = useState(1); // animates 1→2→3→1 every 500ms during update

  useEffect(() => {
    apiClient.me().then(
      (r) => setHubVersion(r.hub_version ?? null),
      () => setHubVersion(null),
    );
  }, []);

  // Probe latest release tag once we know our own version, so we only
  // show the Update button when it'd actually do something. Failures
  // are silent — the button just stays hidden.
  useEffect(() => {
    if (!hubVersion) return;
    apiClient.agents.releases().then(
      (r) => {
        if (!r.latest) return;
        if (compareSemver(r.latest, hubVersion) > 0) {
          setUpdate({ kind: 'available', latest: r.latest });
        }
      },
      () => {},
    );
  }, [hubVersion]);

  // Dot-animation timer while updating or waiting.
  useEffect(() => {
    if (update.kind !== 'updating' && update.kind !== 'waiting') return;
    const t = window.setInterval(() => {
      setDot((d) => (d % 3) + 1);
    }, 500);
    return () => window.clearInterval(t);
  }, [update.kind]);

  // Poll /me while waiting. Track "first time it comes back online" —
  // refresh the page so the new SPA bundle loads.
  const sawDownRef = useRef(false);
  useEffect(() => {
    if (update.kind !== 'waiting') {
      sawDownRef.current = false;
      return;
    }
    const started = Date.now();
    const t = window.setInterval(async () => {
      // Fail-safe: if we never see /me succeed within 60 s, surface an
      // error instead of spinning forever.
      if (Date.now() - started > 60_000) {
        window.clearInterval(t);
        setUpdate({
          kind: 'failed',
          message: 'hub did not come back online within 60s',
        });
        return;
      }
      try {
        await apiClient.me();
        // Once we see at least one failure followed by a success, the
        // hub has restarted with the new binary — reload to pick up
        // the new SPA bundle too.
        if (sawDownRef.current) {
          window.location.reload();
        }
      } catch {
        sawDownRef.current = true;
      }
    }, 1500);
    return () => window.clearInterval(t);
  }, [update.kind]);

  async function handleLogout() {
    try {
      await apiClient.logout();
    } catch {
      /* ignore */
    }
    setOut();
    nav('/login', { replace: true });
  }

  async function handleUpdate() {
    setUpdate({ kind: 'updating' });
    setDot(1);
    try {
      await apiClient.hub.update();
      // 202 returned. The hub exits ~500 ms after this; switch into
      // poll-for-comeback mode.
      setUpdate({ kind: 'waiting' });
    } catch (e: unknown) {
      const msg =
        typeof e === 'object' && e && 'message' in e
          ? String((e as { message?: unknown }).message ?? 'update failed')
          : 'update failed';
      setUpdate({ kind: 'failed', message: msg });
    }
  }

  const animatedLabel =
    update.kind === 'updating' || update.kind === 'waiting'
      ? `Updating${'.'.repeat(dot)}`
      : null;

  return (
    <div className="min-h-full flex flex-col">
      <header className="border-b border-zinc-200 dark:border-zinc-800 px-6 py-3 flex items-center justify-between">
        <div className="flex items-center gap-6">
          <h1 className="font-semibold text-lg flex items-center gap-2">
            <Logo className="h-6 w-6 text-zinc-900 dark:text-zinc-100" />
            <span>CloudCode admin</span>
            {hubVersion && (
              <span
                className="font-mono text-xs font-normal px-1.5 py-0.5 rounded bg-zinc-100 dark:bg-zinc-800 text-zinc-500"
                title="Hub binary version"
              >
                hub {hubVersion}
              </span>
            )}
            {update.kind === 'available' && (
              <button
                onClick={handleUpdate}
                className="font-mono text-xs font-normal px-1.5 py-0.5 rounded border border-amber-400 text-amber-700 dark:text-amber-300 hover:bg-amber-50 dark:hover:bg-amber-900/30"
                title={`Update hub to ${update.latest}`}
              >
                Update → {update.latest}
              </button>
            )}
            {animatedLabel && (
              <span
                className="font-mono text-xs font-normal px-1.5 py-0.5 rounded bg-amber-100 dark:bg-amber-900/40 text-amber-800 dark:text-amber-200 select-none"
                title={
                  update.kind === 'waiting'
                    ? 'Waiting for the hub to come back online'
                    : 'Hub self-update in progress'
                }
              >
                {animatedLabel}
              </span>
            )}
            {update.kind === 'failed' && (
              <span
                className="font-mono text-xs font-normal px-1.5 py-0.5 rounded bg-red-100 dark:bg-red-900/40 text-red-800 dark:text-red-200"
                title={update.message}
              >
                Update failed
              </span>
            )}
          </h1>
          <nav className="flex gap-4 text-sm">
            <Tab to="/" end>
              Dashboard
            </Tab>
            <Tab to="/accounts">Accounts</Tab>
            <Tab to="/agents">Agents</Tab>
            <Tab to="/workspaces">Workspaces</Tab>
            <Tab to="/sessions">Sessions</Tab>
            <Tab to="/audit">Audit</Tab>
          </nav>
        </div>
        <div className="flex items-center gap-2">
          <button
            onClick={() => setSettingsOpen(true)}
            className="text-sm px-3 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
            title="Admin settings"
          >
            Settings
          </button>
          <button
            onClick={handleLogout}
            className="text-sm px-3 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
          >
            Sign out
          </button>
        </div>
      </header>
      <main className="flex-1 px-6 py-6 max-w-screen-xl w-full mx-auto">
        <Outlet />
      </main>
      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} />
    </div>
  );
}

function Tab({ to, children, end }: { to: string; children: React.ReactNode; end?: boolean }) {
  return (
    <NavLink
      to={to}
      end={end}
      className={({ isActive }) =>
        `px-2 py-1 rounded ${
          isActive
            ? 'bg-zinc-200 dark:bg-zinc-800 text-zinc-900 dark:text-zinc-100'
            : 'text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100'
        }`
      }
    >
      {children}
    </NavLink>
  );
}
