// Theme + global env vars + per-tool default args modal.

import { apply, getStoredTheme, setStoredTheme, Theme } from '@/lib/theme';
import { useState } from 'react';
import { type Tool } from '@/lib/tools';
import { type Preferences } from '@/lib/preferences';
import {
  ArgsEditor,
  EnvEditor,
  collapseConfig,
  envToRows,
  initialArgsText,
  invalidRowIds,
  type EnvRow,
} from '@/components/ConfigForm';

type Props = {
  onClose: () => void;
  /** Called whenever the theme is changed so callers can react (e.g. update terminals). */
  onThemeChange?: (t: Theme) => void;
  preferences: Preferences;
  onSavePreferences: (next: Preferences) => void;
  realName?: string | null;
  onSaveRealName?: (name: string | null) => void;
  /** Closes settings and re-runs the first-time tour. */
  onReplayTutorial?: () => void;
};

export default function SettingsDialog({
  onClose,
  onThemeChange,
  preferences,
  onSavePreferences,
  realName,
  onSaveRealName,
  onReplayTutorial,
}: Props) {
  const [theme, setTheme] = useState<Theme>(getStoredTheme);
  const [nameText, setNameText] = useState(realName ?? '');
  // Local form state, seeded from props at mount. We commit (parse +
  // save) on Save rather than per-keystroke so partial typing doesn't
  // round-trip through the server.
  const [envRows, setEnvRows] = useState<EnvRow[]>(() =>
    envToRows(preferences.env),
  );
  const [argsText, setArgsText] = useState<Record<Tool, string>>(() =>
    initialArgsText(preferences),
  );

  const hasInvalidEnv = invalidRowIds(envRows).size > 0;

  function handleTheme(t: Theme) {
    setTheme(t);
    apply(t);
  }

  function handleSave() {
    if (hasInvalidEnv) return;
    commitName();
    const next: Preferences = {
      ...preferences,
      ...collapseConfig(preferences, envRows, argsText),
    };
    onSavePreferences(next);
    setStoredTheme(theme);
    onThemeChange?.(theme);
    onClose();
  }

  function handleClose() {
    const stored = getStoredTheme();
    apply(stored);
    onThemeChange?.(stored);
    onClose();
  }

  function commitName() {
    const trimmed = nameText.trim();
    const next = trimmed || null;
    if (next === (realName ?? null)) return;
    onSaveRealName?.(next);
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40">
      <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-xl p-6 w-full max-w-md mx-4 max-h-[90vh] overflow-y-auto">
        <h3 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 mb-4">
          Settings
        </h3>

        <div className="mb-5">
          <label className="block text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-2 uppercase tracking-wide">
            Real Name
          </label>
          <input
            type="text"
            spellCheck={false}
            placeholder="Your display name"
            value={nameText}
            onChange={(e) => setNameText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') handleSave();
            }}
            className="w-full px-2 py-1.5 text-sm rounded-md border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-950 text-zinc-900 dark:text-zinc-100 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:focus:ring-zinc-500"
          />
        </div>

        <div className="mb-5">
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

        <div className="mb-5">
          <ArgsEditor
            argsText={argsText}
            onChange={(tool, text) =>
              setArgsText((prev) => ({ ...prev, [tool]: text }))
            }
          />
        </div>

        <div className="mb-5">
          <EnvEditor rows={envRows} onChange={setEnvRows} />
        </div>

        <div className="flex items-center gap-2">
          {onReplayTutorial && (
            <button
              type="button"
              onClick={() => {
                onReplayTutorial();
                onClose();
              }}
              className="text-xs text-zinc-500 dark:text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100 underline transition-colors"
            >
              Show tutorial again
            </button>
          )}
          <div className="flex-1" />
          <button
            onClick={handleClose}
            className="text-sm px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
          >
            Cancel
          </button>
          <button
            onClick={handleSave}
            disabled={hasInvalidEnv}
            className="text-sm px-3 py-1.5 rounded-lg bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
