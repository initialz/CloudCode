import { useEffect, useState, FormEvent } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import Logo from '@/components/Logo';
import { acceptInvite, getInviteInfo, type InviteInfo } from '@/lib/api';

const USERNAME_RE = /^[A-Za-z0-9_-]{1,64}$/;

type LoadState =
  | { kind: 'loading' }
  | { kind: 'fetch_error'; message: string }
  | { kind: 'invalid'; reason: string }
  | { kind: 'valid'; info: Extract<InviteInfo, { valid: true }> };

type Created = {
  account: string;
  token: string;
};

function reasonMessage(reason: string): string {
  switch (reason) {
    case 'not_found':
      return 'This invite link is invalid or has expired.';
    case 'inactive':
      return 'This invite link has been disabled.';
    case 'exhausted':
      return 'This invite link has reached its usage limit.';
    default:
      return 'This invite link cannot be used.';
  }
}

export default function Invite() {
  const { token: inviteToken } = useParams<{ token: string }>();
  const navigate = useNavigate();

  const [state, setState] = useState<LoadState>({ kind: 'loading' });
  const [username, setUsername] = useState('');
  const [realName, setRealName] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState('');
  const [created, setCreated] = useState<Created | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let cancelled = false;
    if (!inviteToken) {
      setState({ kind: 'invalid', reason: 'not_found' });
      return;
    }
    setState({ kind: 'loading' });
    getInviteInfo(inviteToken)
      .then((info) => {
        if (cancelled) return;
        if (info.valid) {
          setState({ kind: 'valid', info });
        } else {
          setState({ kind: 'invalid', reason: info.reason });
        }
      })
      .catch((err) => {
        if (cancelled) return;
        setState({
          kind: 'fetch_error',
          message: err instanceof Error ? err.message : 'Failed to load invite',
        });
      });
    return () => {
      cancelled = true;
    };
  }, [inviteToken]);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!inviteToken) return;
    const trimmed = username.trim();
    if (!USERNAME_RE.test(trimmed)) {
      setSubmitError(
        'Username must be 1–64 characters, letters/numbers/_/- only.',
      );
      return;
    }
    setSubmitError('');
    setSubmitting(true);
    try {
      const trimmedRealName = realName.trim();
      const result = await acceptInvite(
        inviteToken,
        trimmed,
        trimmedRealName || null,
      );
      setCreated(result);
    } catch (err) {
      setSubmitError(
        err instanceof Error ? err.message : 'Failed to create account',
      );
    } finally {
      setSubmitting(false);
    }
  }

  async function copyToken() {
    if (!created) return;
    try {
      await navigator.clipboard.writeText(created.token);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Fallback: select-on-focus already makes manual copy easy.
    }
  }

  function goToLogin() {
    if (created) {
      navigate(`/login?username=${encodeURIComponent(created.account)}`);
    } else {
      navigate('/login');
    }
  }

  // ── Render ────────────────────────────────────────────────────────────────

  return (
    <div className="min-h-full flex items-center justify-center px-4 py-10">
      <div className="w-full max-w-md">
        <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-sm p-8">
          {/* Header */}
          <div className="flex flex-col items-center gap-3 mb-6">
            <Logo size={48} className="text-zinc-900 dark:text-zinc-100" />
            <div className="text-center">
              <h1 className="text-xl font-semibold text-zinc-900 dark:text-zinc-100">
                cloudcode
              </h1>
            </div>
          </div>

          {state.kind === 'loading' && (
            <div className="text-center text-sm text-zinc-500 dark:text-zinc-400 py-6">
              Loading invite…
            </div>
          )}

          {state.kind === 'fetch_error' && (
            <div className="rounded-lg bg-red-50 dark:bg-red-950 border border-red-200 dark:border-red-900 px-4 py-3 text-sm text-red-700 dark:text-red-400">
              {state.message}
            </div>
          )}

          {state.kind === 'invalid' && (
            <div className="flex flex-col items-center gap-4 py-2">
              <div className="w-full rounded-lg bg-red-50 dark:bg-red-950 border border-red-200 dark:border-red-900 px-4 py-3 text-sm text-red-700 dark:text-red-400 text-center">
                {reasonMessage(state.reason)}
              </div>
              <Link
                to="/login"
                className="text-sm text-zinc-600 dark:text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100 underline"
              >
                Go to login
              </Link>
            </div>
          )}

          {state.kind === 'valid' && !created && (
            <ValidInviteForm
              username={username}
              setUsername={setUsername}
              realName={realName}
              setRealName={setRealName}
              submitting={submitting}
              submitError={submitError}
              onSubmit={handleSubmit}
            />
          )}

          {state.kind === 'valid' && created && (
            <SuccessPanel
              created={created}
              copied={copied}
              onCopy={copyToken}
              onContinue={goToLogin}
            />
          )}
        </div>
      </div>
    </div>
  );
}

// ── Subcomponents ───────────────────────────────────────────────────────────

type ValidProps = {
  username: string;
  setUsername: (v: string) => void;
  realName: string;
  setRealName: (v: string) => void;
  submitting: boolean;
  submitError: string;
  onSubmit: (e: FormEvent) => void;
};

function ValidInviteForm({
  username,
  setUsername,
  realName,
  setRealName,
  submitting,
  submitError,
  onSubmit,
}: ValidProps) {
  const trimmed = username.trim();
  const canSubmit =
    !submitting && trimmed.length > 0 && USERNAME_RE.test(trimmed);

  return (
    <>
      <div className="text-center mb-5">
        <h2 className="text-base font-semibold text-zinc-900 dark:text-zinc-100">
          You've been invited to cloudcode
        </h2>
        <p className="text-sm text-zinc-500 dark:text-zinc-400 mt-1">
          Fill in your details to create your account.
        </p>
      </div>

      {submitError && (
        <div className="mb-4 rounded-lg bg-red-50 dark:bg-red-950 border border-red-200 dark:border-red-900 px-4 py-3 text-sm text-red-700 dark:text-red-400">
          {submitError}
        </div>
      )}

      <form onSubmit={onSubmit} className="flex flex-col gap-4">
        <div>
          <label
            htmlFor="realName"
            className="block text-sm font-medium text-zinc-700 dark:text-zinc-300 mb-1.5"
          >
            Real name <span className="text-xs text-zinc-400 dark:text-zinc-500 font-normal">(optional)</span>
          </label>
          <input
            id="realName"
            type="text"
            autoComplete="name"
            spellCheck={false}
            placeholder="Your display name"
            value={realName}
            onChange={(e) => setRealName(e.target.value)}
            disabled={submitting}
            maxLength={128}
            className="w-full rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-800 px-3 py-2 text-sm text-zinc-900 dark:text-zinc-100 placeholder-zinc-400 dark:placeholder-zinc-500 focus:outline-none focus:ring-2 focus:ring-zinc-500 dark:focus:ring-zinc-400 disabled:opacity-50"
          />
        </div>

        <div>
          <label
            htmlFor="username"
            className="block text-sm font-medium text-zinc-700 dark:text-zinc-300 mb-1.5"
          >
            Username
          </label>
          <input
            id="username"
            type="text"
            autoComplete="username"
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            placeholder="your-username"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            disabled={submitting}
            autoFocus
            className="w-full rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-800 px-3 py-2 text-sm text-zinc-900 dark:text-zinc-100 placeholder-zinc-400 dark:placeholder-zinc-500 focus:outline-none focus:ring-2 focus:ring-zinc-500 dark:focus:ring-zinc-400 disabled:opacity-50"
          />
          <p className="mt-1.5 text-xs text-zinc-400 dark:text-zinc-500">
            Letters, numbers, underscores, and dashes. 1–64 characters.
          </p>
        </div>

        <button
          type="submit"
          disabled={!canSubmit}
          className="w-full rounded-lg bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 text-sm font-medium py-2 px-4 hover:bg-zinc-700 dark:hover:bg-zinc-300 focus:outline-none focus:ring-2 focus:ring-zinc-500 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
        >
          {submitting ? 'Creating account…' : 'Create account'}
        </button>
      </form>
    </>
  );
}

type SuccessProps = {
  created: Created;
  copied: boolean;
  onCopy: () => void;
  onContinue: () => void;
};

function SuccessPanel({ created, copied, onCopy, onContinue }: SuccessProps) {
  return (
    <div className="flex flex-col gap-4">
      <div className="rounded-lg bg-green-50 dark:bg-green-950 border border-green-200 dark:border-green-900 px-4 py-3 text-center">
        <div className="text-base font-semibold text-green-800 dark:text-green-300">
          Account created!
        </div>
        <div className="mt-0.5 text-xs text-green-700 dark:text-green-400">
          Welcome to cloudcode.
        </div>
      </div>

      <div>
        <div className="text-xs font-medium text-zinc-600 dark:text-zinc-400 mb-1">
          Username
        </div>
        <div className="rounded-lg border border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-800/50 px-3 py-2 text-sm font-mono text-zinc-900 dark:text-zinc-100 break-all">
          {created.account}
        </div>
      </div>

      <div>
        <div className="text-xs font-medium text-zinc-600 dark:text-zinc-400 mb-1">
          Account token
        </div>
        <div className="flex gap-2">
          <input
            readOnly
            value={created.token}
            onFocus={(e) => e.currentTarget.select()}
            className="flex-1 min-w-0 rounded-lg border border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-800/50 px-3 py-2 text-sm font-mono text-zinc-900 dark:text-zinc-100 focus:outline-none focus:ring-2 focus:ring-zinc-500"
          />
          <button
            type="button"
            onClick={onCopy}
            className="shrink-0 rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 text-zinc-700 dark:text-zinc-200 text-sm font-medium px-3 py-2 hover:bg-zinc-100 dark:hover:bg-zinc-800 transition-colors"
          >
            {copied ? 'Copied' : 'Copy'}
          </button>
        </div>
      </div>

      <div className="rounded-lg bg-amber-50 dark:bg-amber-950 border border-amber-200 dark:border-amber-900 px-4 py-3 text-xs text-amber-800 dark:text-amber-300">
        Save this token now — you won't see it again.
      </div>

      <button
        type="button"
        onClick={onContinue}
        className="w-full rounded-lg bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 text-sm font-medium py-2 px-4 hover:bg-zinc-700 dark:hover:bg-zinc-300 focus:outline-none focus:ring-2 focus:ring-zinc-500 transition-colors"
      >
        Continue to login
      </button>
    </div>
  );
}
