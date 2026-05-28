import { useEffect, useState } from 'react';
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

  // Probe the latest release tag periodically so the Update badge
  // shows up shortly after a new tag is published, not just at page
  // load. We skip re-polling while an update is in flight (avoids
  // racing with the `updating` / `waiting` state). TEMP cadence —
  // restore to a longer interval (or back to mount-only) after the
  // self-update test cycle.
  useEffect(() => {
    if (!hubVersion) return;
    const check = () => {
      apiClient.agents.releases().then(
        (r) => {
          if (!r.latest) return;
          const latest = r.latest;
          setUpdate((cur) => {
            if (cur.kind === 'updating' || cur.kind === 'waiting' || cur.kind === 'failed') {
              return cur;
            }
            return compareSemver(latest, hubVersion) > 0
              ? { kind: 'available', latest }
              : { kind: 'idle' };
          });
        },
        () => {},
      );
    };
    check();
    const t = window.setInterval(check, 60_000);
    return () => window.clearInterval(t);
  }, [hubVersion]);

  // Dot-animation timer while updating or waiting.
  useEffect(() => {
    if (update.kind !== 'updating' && update.kind !== 'waiting') return;
    const t = window.setInterval(() => {
      setDot((d) => (d % 3) + 1);
    }, 500);
    return () => window.clearInterval(t);
  }, [update.kind]);

  // Poll the public /hub-version endpoint while waiting. We hit a
  // dedicated unauthenticated endpoint instead of /me because the
  // hub restart wipes the in-memory cookie session — so /me would
  // 401 forever from this tab's POV, and the page would never know
  // the hub came back. Wait for the version string to flip to
  // anything strictly greater than what was shown in the header
  // before we kicked off the update.
  useEffect(() => {
    if (update.kind !== 'waiting') return;
    const started = Date.now();
    const t = window.setInterval(async () => {
      // Fail-safe: 90 s should be plenty for "download already on
      // disk, supervisor exec, hub re-bind". If we exceed it,
      // surface an error instead of spinning forever.
      if (Date.now() - started > 90_000) {
        window.clearInterval(t);
        setUpdate({
          kind: 'failed',
          message: 'hub did not come back online within 90s',
        });
        return;
      }
      try {
        const { version } = await apiClient.hub.version();
        // New version is anything strictly newer than the
        // pre-update header value. compareSemver tolerates the
        // leading "v" prefix.
        if (hubVersion && compareSemver(version, hubVersion) > 0) {
          window.location.reload();
        }
      } catch {
        // Hub still restarting — keep polling.
      }
    }, 1500);
    return () => window.clearInterval(t);
  }, [update.kind, hubVersion]);

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

  const isAnimating = update.kind === 'updating' || update.kind === 'waiting';

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
            {isAnimating && (
              <span
                className="font-mono text-xs font-normal px-1.5 py-0.5 rounded bg-amber-100 dark:bg-amber-900/40 text-amber-800 dark:text-amber-200 select-none"
                title={
                  update.kind === 'waiting'
                    ? 'Waiting for the hub to come back online'
                    : 'Hub self-update in progress'
                }
              >
                Updating
                {/* Fixed-width box for the dot animation so the
                    badge doesn't reflow as the dot count changes. */}
                <span
                  className="inline-block text-left"
                  style={{ width: '1.5ch' }}
                  aria-hidden
                >
                  {'.'.repeat(dot)}
                </span>
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
            <Tab to="/activity">Activity</Tab>
            <Tab to="/invites">Invites</Tab>
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
