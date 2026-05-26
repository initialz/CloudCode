import { useEffect, useState } from 'react';
import { Modal } from './Modal';
import {
  getStoredHubUrl,
  guessHubUrl,
  setStoredHubUrl,
} from '@/lib/hubUrl';
import { apply, getStoredTheme, setStoredTheme, type Theme } from '@/lib/theme';

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

  useEffect(() => {
    if (open) {
      setHubUrl(getStoredHubUrl());
      setTheme(getStoredTheme());
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
      </div>
    </Modal>
  );
}
