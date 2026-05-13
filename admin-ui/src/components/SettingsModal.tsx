import { useEffect, useState } from 'react';
import { Modal } from './Modal';
import {
  getStoredHubUrl,
  guessHubUrl,
  setStoredHubUrl,
} from '@/lib/hubUrl';

export function SettingsModal({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const [hubUrl, setHubUrl] = useState('');
  const guessed = guessHubUrl();

  useEffect(() => {
    if (open) setHubUrl(getStoredHubUrl());
  }, [open]);

  function save() {
    setStoredHubUrl(hubUrl);
    onClose();
  }

  function reset() {
    setStoredHubUrl('');
    setHubUrl('');
  }

  return (
    <Modal
      open={open}
      onClose={onClose}
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
            onClick={onClose}
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
          Used in the install one-liner the admin UI gives out when you
          create or rotate an account token. Leave blank to use the
          auto-detected default <code>{guessed}</code>. Saved locally in
          this browser (localStorage).
        </p>
      </div>
    </Modal>
  );
}
