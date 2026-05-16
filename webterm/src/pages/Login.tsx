import { useState, FormEvent } from 'react';
import { useNavigate } from 'react-router-dom';
import Logo from '@/components/Logo';
import { apiClient, ApiError } from '@/lib/api';

export default function Login() {
  const navigate = useNavigate();
  const [token, setToken] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!token.trim()) return;
    setError('');
    setLoading(true);
    try {
      await apiClient.login(token.trim());
      navigate('/');
    } catch (err) {
      const ae = err as ApiError;
      setError(ae.message ?? 'Login failed');
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="min-h-full flex items-center justify-center px-4">
      <div className="w-full max-w-sm">
        {/* Card */}
        <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-sm p-8">
          {/* Header */}
          <div className="flex flex-col items-center gap-3 mb-8">
            <Logo size={48} className="text-zinc-900 dark:text-zinc-100" />
            <div className="text-center">
              <h1 className="text-xl font-semibold text-zinc-900 dark:text-zinc-100">
                cloudcode
              </h1>
              <p className="text-sm text-zinc-500 dark:text-zinc-400 mt-0.5">
                Sign in with your account token
              </p>
            </div>
          </div>

          {/* Error banner */}
          {error && (
            <div className="mb-4 rounded-lg bg-red-50 dark:bg-red-950 border border-red-200 dark:border-red-900 px-4 py-3 text-sm text-red-700 dark:text-red-400">
              {error}
            </div>
          )}

          <form onSubmit={handleSubmit} className="flex flex-col gap-4">
            <div>
              <label
                htmlFor="token"
                className="block text-sm font-medium text-zinc-700 dark:text-zinc-300 mb-1.5"
              >
                Account token
              </label>
              <input
                id="token"
                type="password"
                autoComplete="current-password"
                placeholder="cc_..."
                value={token}
                onChange={(e) => setToken(e.target.value)}
                disabled={loading}
                className="w-full rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-800 px-3 py-2 text-sm text-zinc-900 dark:text-zinc-100 placeholder-zinc-400 dark:placeholder-zinc-500 focus:outline-none focus:ring-2 focus:ring-zinc-500 dark:focus:ring-zinc-400 disabled:opacity-50"
              />
            </div>

            <button
              type="submit"
              disabled={loading || !token.trim()}
              className="w-full rounded-lg bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 text-sm font-medium py-2 px-4 hover:bg-zinc-700 dark:hover:bg-zinc-300 focus:outline-none focus:ring-2 focus:ring-zinc-500 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
            >
              {loading ? 'Signing in...' : 'Sign in'}
            </button>
          </form>
        </div>
      </div>
    </div>
  );
}
