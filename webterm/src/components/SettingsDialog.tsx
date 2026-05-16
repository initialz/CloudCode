// Theme-settings modal, extracted from Picker/Session.

import { getStoredTheme, setStoredTheme, Theme } from '@/lib/theme';
import { useState } from 'react';

type Props = {
  onClose: () => void;
  /** Called whenever the theme is changed so callers can react (e.g. update terminals). */
  onThemeChange?: (t: Theme) => void;
};

export default function SettingsDialog({ onClose, onThemeChange }: Props) {
  const [theme, setTheme] = useState<Theme>(getStoredTheme);

  function handleTheme(t: Theme) {
    setTheme(t);
    setStoredTheme(t);
    onThemeChange?.(t);
  }

  return (
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
            onClick={onClose}
            className="text-sm px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
          >
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
