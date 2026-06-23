import { useEffect, useState } from 'react';
import { Modal } from './Modal';
import {
  getStoredHubUrl,
  guessHubUrl,
  setStoredHubUrl,
} from '@/lib/hubUrl';
import { apply, getStoredTheme, setStoredTheme, type Theme } from '@/lib/theme';
import {
  apiClient,
  type CleanupPreviewDto,
  type CleanupResultDto,
} from '@/lib/api';

const RETENTION_OPTIONS: { months: number; label: string }[] = [
  { months: 1, label: '1 month' },
  { months: 3, label: '3 months' },
  { months: 6, label: '6 months' },
  { months: 12, label: '1 year' },
];

export function SettingsModal({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const [hubUrl, setHubUrl] = useState('');
  const [theme, setTheme] = useState<Theme>('system');
  const guessed = guessHubUrl();

  // ── Data maintenance (server-side, independent of Save) ────────────────
  const [months, setMonths] = useState(6);
  const [preview, setPreview] = useState<CleanupPreviewDto | null>(null);
  const [result, setResult] = useState<CleanupResultDto | null>(null);
  const [busy, setBusy] = useState<'idle' | 'previewing' | 'deleting'>('idle');
  const [confirming, setConfirming] = useState(false);
  const [cleanupErr, setCleanupErr] = useState<string | null>(null);

  // Any change to the window invalidates a stale preview/confirm.
  function pickMonths(m: number) {
    setMonths(m);
    setPreview(null);
    setResult(null);
    setConfirming(false);
    setCleanupErr(null);
  }

  async function doPreview() {
    setBusy('previewing');
    setCleanupErr(null);
    setResult(null);
    setConfirming(false);
    try {
      setPreview(await apiClient.maintenance.cleanupPreview(months));
    } catch (e: unknown) {
      setCleanupErr(e instanceof Error ? e.message : 'preview failed');
    } finally {
      setBusy('idle');
    }
  }

  async function doCleanup() {
    if (!confirming) {
      setConfirming(true);
      return;
    }
    setBusy('deleting');
    setCleanupErr(null);
    try {
      const r = await apiClient.maintenance.cleanup(months, true);
      setResult(r);
      setPreview(null);
    } catch (e: unknown) {
      setCleanupErr(e instanceof Error ? e.message : 'cleanup failed');
    } finally {
      setBusy('idle');
      setConfirming(false);
    }
  }

  useEffect(() => {
    if (open) {
      setHubUrl(getStoredHubUrl());
      setTheme(getStoredTheme());
      // Reset the maintenance panel each time the modal opens.
      setPreview(null);
      setResult(null);
      setConfirming(false);
      setCleanupErr(null);
      setBusy('idle');
    }
  }, [open]);

  function handleClose() {
    apply(getStoredTheme());
    onClose();
  }

  function save() {
    setStoredHubUrl(hubUrl);
    setStoredTheme(theme);
    onClose();
  }

  function reset() {
    setStoredHubUrl('');
    setHubUrl('');
    const t = 'system' as Theme;
    setStoredTheme(t);
    setTheme(t);
  }

  return (
    <Modal
      open={open}
      onClose={handleClose}
      title="Admin settings"
      footer={
        <>
          <button
            onClick={reset}
            className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
          >
            Reset to default
          </button>
          <button
            onClick={handleClose}
            className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
          >
            Cancel
          </button>
          <button
            onClick={save}
            className="px-3 py-1.5 text-sm rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90"
          >
            Save
          </button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="space-y-2">
          <label className="block text-sm font-medium">Public hub URL</label>
          <input
            type="text"
            value={hubUrl}
            onChange={(e) => setHubUrl(e.target.value)}
            placeholder={guessed}
            className="w-full px-3 py-2 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent text-sm font-mono focus:outline-none focus:ring-2 focus:ring-zinc-400"
          />
          <p className="text-xs text-zinc-500">
            Used in the install one-liner the admin UI gives out when
            you create or rotate an account token. Leave blank to use
            the auto-detected default <code>{guessed}</code>. Saved
            locally in this browser (localStorage).
          </p>
        </div>

        <div className="space-y-2">
          <label className="block text-sm font-medium">Theme</label>
          <div className="flex gap-3 text-sm">
            {(['system', 'light', 'dark'] as const).map((opt) => (
              <label
                key={opt}
                className={`flex items-center gap-2 px-3 py-1.5 rounded border cursor-pointer ${
                  theme === opt
                    ? 'border-zinc-900 dark:border-zinc-100 bg-zinc-100 dark:bg-zinc-800'
                    : 'border-zinc-300 dark:border-zinc-700 hover:bg-zinc-50 dark:hover:bg-zinc-900/50'
                }`}
              >
                <input
                  type="radio"
                  name="theme"
                  value={opt}
                  checked={theme === opt}
                  onChange={() => { setTheme(opt); apply(opt); }}
                  className="sr-only"
                />
                <span className="capitalize">{opt}</span>
              </label>
            ))}
          </div>
          <p className="text-xs text-zinc-500">
            <code>System</code> follows your OS appearance setting and
            switches live. Saved locally in this browser.
          </p>
        </div>

        {/* Data maintenance — server-side, runs immediately (independent of
            the Save button below). */}
        <div className="space-y-2 pt-4 border-t border-zinc-200 dark:border-zinc-800">
          <label className="block text-sm font-medium text-red-700 dark:text-red-400">
            Data maintenance
          </label>
          <p className="text-xs text-zinc-500">
            Permanently delete conversation messages, sessions, and
            interaction history older than the selected age, then reclaim disk
            (VACUUM). The audit trail and your accounts / workspaces are kept.
            This runs on the hub immediately and cannot be undone.
          </p>

          <div className="flex flex-wrap items-center gap-2 pt-1">
            <select
              value={months}
              onChange={(e) => pickMonths(Number(e.target.value))}
              disabled={busy !== 'idle'}
              className="px-2 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent text-sm"
            >
              {RETENTION_OPTIONS.map((o) => (
                <option key={o.months} value={o.months}>
                  older than {o.label}
                </option>
              ))}
            </select>
            <button
              onClick={doPreview}
              disabled={busy !== 'idle'}
              className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
            >
              {busy === 'previewing' ? 'Checking…' : 'Preview'}
            </button>
            <button
              onClick={doCleanup}
              disabled={busy !== 'idle' || !preview}
              className={`px-3 py-1.5 text-sm rounded text-white disabled:opacity-50 ${
                confirming
                  ? 'bg-red-700 hover:bg-red-800'
                  : 'bg-red-600 hover:bg-red-700'
              }`}
            >
              {busy === 'deleting'
                ? 'Deleting…'
                : confirming
                  ? 'Click again to permanently delete'
                  : 'Delete & reclaim space'}
            </button>
          </div>

          {preview && (
            <div className="text-xs text-zinc-600 dark:text-zinc-300 rounded border border-amber-300 dark:border-amber-800/60 bg-amber-50 dark:bg-amber-950/30 px-3 py-2">
              Older than{' '}
              <strong>{new Date(preview.cutoff * 1000).toLocaleDateString()}</strong>{' '}
              this will delete:{' '}
              <strong>{preview.messages.toLocaleString()}</strong> messages,{' '}
              <strong>{preview.sessions.toLocaleString()}</strong> sessions,{' '}
              <strong>{preview.user_interactions.toLocaleString()}</strong>{' '}
              interactions.
              {preview.messages + preview.sessions + preview.user_interactions ===
                0 && ' (nothing to delete)'}
            </div>
          )}

          {result && (
            <div className="text-xs text-emerald-700 dark:text-emerald-400 rounded border border-emerald-300 dark:border-emerald-800/60 bg-emerald-50 dark:bg-emerald-950/30 px-3 py-2">
              Deleted{' '}
              <strong>{result.deleted_messages.toLocaleString()}</strong> messages,{' '}
              <strong>{result.deleted_sessions.toLocaleString()}</strong> sessions,{' '}
              <strong>{result.deleted_user_interactions.toLocaleString()}</strong>{' '}
              interactions
              {result.vacuumed ? ' · disk reclaimed (VACUUM)' : ''}.
            </div>
          )}

          {cleanupErr && (
            <div className="text-xs text-red-700 dark:text-red-400 rounded border border-red-300 dark:border-red-800/60 bg-red-50 dark:bg-red-950/30 px-3 py-2">
              {cleanupErr}
            </div>
          )}

          {busy === 'deleting' && (
            <p className="text-xs text-zinc-500">
              VACUUM briefly pauses hub writes while it rewrites the database
              file — this can take a while on a large database.
            </p>
          )}
        </div>
      </div>
    </Modal>
  );
}
