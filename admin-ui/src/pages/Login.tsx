import { useState, type FormEvent } from 'react';
import { useNavigate, useLocation, Navigate } from 'react-router-dom';
import { apiClient } from '@/lib/api';
import { useAuth } from '@/lib/auth';

export function Login() {
  const [token, setToken] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const nav = useNavigate();
  const loc = useLocation();
  const { status, setIn } = useAuth();

  if (status === 'in') {
    const dest = (loc.state as any)?.from?.pathname ?? '/';
    return <Navigate to={dest} replace />;
  }

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await apiClient.login(token.trim());
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
        <div>
          <h1 className="text-lg font-semibold">CloudCode admin</h1>
          <p className="text-sm text-zinc-500 mt-1">Sign in with the admin token.</p>
        </div>

        {error && (
          <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
            {error}
          </div>
        )}

        <label className="block">
          <span className="text-sm text-zinc-700 dark:text-zinc-300">Admin token</span>
          <input
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoFocus
            required
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
          The plaintext token was printed once by <code>cloudcode-hub --init</code>.
        </p>
      </form>
    </div>
  );
}
