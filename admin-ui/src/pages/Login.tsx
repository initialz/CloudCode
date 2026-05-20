import { useEffect, useState, type FormEvent } from 'react';
import { useNavigate, useLocation, Navigate } from 'react-router-dom';
import { apiClient } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import { Logo } from '@/components/Logo';

export function Login() {
  // Username is fetched from the unauth /login-info endpoint and
  // rendered as a disabled input — admin is a single-account
  // identity, the operator doesn't pick it. `[admin].username` in
  // hub.toml is the source of truth; falling back to "admin" if the
  // fetch fails (older hub without the endpoint) keeps existing
  // installs working.
  const [username, setUsername] = useState('admin');
  const [token, setToken] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const nav = useNavigate();
  const loc = useLocation();
  const { status, setIn } = useAuth();

  useEffect(() => {
    apiClient
      .loginInfo()
      .then((info) => {
        // Guard against an older hub that doesn't have the endpoint
        // and returns the SPA HTML shell (content-type text/html) —
        // our api() helper hands back the raw string in that case,
        // and `info.username` would silently become undefined and
        // poison the state. Only accept a real `{username: string}`.
        if (info && typeof info.username === 'string') {
          setUsername(info.username);
        }
      })
      .catch(() => {
        // Network error — leave the "admin" default; the login
        // submit will fail loudly if the configured username is
        // actually something else.
      });
  }, []);

  if (status === 'in') {
    const dest = (loc.state as any)?.from?.pathname ?? '/';
    return <Navigate to={dest} replace />;
  }

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      // Defensive: if anything (older cached bundle, network race)
      // managed to clear username state, fall back to "admin" so
      // we never crash with "trim of undefined". The backend will
      // still reject if the actual config differs.
      const u = (username ?? 'admin').trim();
      const t = (token ?? '').trim();
      await apiClient.login(u, t);
      setIn();
      const dest = (loc.state as any)?.from?.pathname ?? '/';
      nav(dest, { replace: true });
    } catch (err: any) {
      setError(err?.message ?? 'login failed');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="min-h-full flex items-center justify-center px-4">
      <form
        onSubmit={onSubmit}
        className="w-full max-w-sm space-y-4 p-6 rounded-lg border border-zinc-200 dark:border-zinc-800 bg-white dark:bg-zinc-900 shadow-sm"
      >
        <div className="flex items-center gap-3">
          <Logo className="h-10 w-10 text-zinc-900 dark:text-zinc-100" />
          <div>
            <h1 className="text-lg font-semibold">CloudCode admin</h1>
            <p className="text-sm text-zinc-500 mt-1">Sign in with the admin username and token.</p>
          </div>
        </div>

        {error && (
          <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
            {error}
          </div>
        )}

        <label className="block">
          <span className="text-sm text-zinc-700 dark:text-zinc-300">Username</span>
          <input
            type="text"
            value={username}
            readOnly
            disabled
            tabIndex={-1}
            autoComplete="username"
            className="mt-1 w-full px-3 py-2 rounded border border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-800/50 text-sm text-zinc-500 dark:text-zinc-400 cursor-not-allowed"
          />
        </label>

        <label className="block">
          <span className="text-sm text-zinc-700 dark:text-zinc-300">Admin token</span>
          <input
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            required
            autoFocus
            autoComplete="current-password"
            className="mt-1 w-full px-3 py-2 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent text-sm focus:outline-none focus:ring-2 focus:ring-zinc-400"
          />
        </label>

        <button
          type="submit"
          disabled={busy || !token.trim()}
          className="w-full py-2 rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 text-sm font-medium hover:opacity-90 disabled:opacity-50"
        >
          {busy ? 'Signing in…' : 'Sign in'}
        </button>

        <p className="text-xs text-zinc-500">
          The username comes from <code>[admin].username</code> in hub.toml
          (default <code>admin</code>). The plaintext token was printed once
          by <code>cloudcode-hub --init</code>.
        </p>
      </form>
    </div>
  );
}
