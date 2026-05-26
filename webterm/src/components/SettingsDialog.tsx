// Theme + per-tool default args modal.

import { apply, getStoredTheme, setStoredTheme, Theme } from '@/lib/theme';
import { useState } from 'react';
import { KNOWN_TOOLS, type Tool } from '@/lib/tools';
import { argsToText, textToArgs, type Preferences } from '@/lib/preferences';

// Tools whose default args we expose in this dialog. KNOWN_TOOLS stays
// the source of truth for "can be launched"; this is a narrower set —
// codex args are hidden until we have a clear UX for codex-specific
// flags.
const CONFIGURABLE_TOOLS: Tool[] = KNOWN_TOOLS.filter((t) => t !== 'codex');

type Props = {
  onClose: () => void;
  /** Called whenever the theme is changed so callers can react (e.g. update terminals). */
  onThemeChange?: (t: Theme) => void;
  preferences: Preferences;
  onSavePreferences: (next: Preferences) => void;
  realName?: string | null;
  onSaveRealName?: (name: string | null) => void;
};

export default function SettingsDialog({
  onClose,
  onThemeChange,
  preferences,
  onSavePreferences,
  realName,
  onSaveRealName,
}: Props) {
  const [theme, setTheme] = useState<Theme>(getStoredTheme);
  const [nameText, setNameText] = useState(realName ?? '');
  // Local text-input state per tool. We commit (parse + save) on blur
  // rather than on every keystroke so partial typing like "--mod" doesn't
  // round-trip through the server. Initial value pulls from the prop;
  // the textbox is otherwise uncontrolled-ish (we hold it in local state
  // and sync from props only at mount).
  const [argsText, setArgsText] = useState<Record<Tool, string>>(() =>
    Object.fromEntries(
      CONFIGURABLE_TOOLS.map((t) => [t, argsToText(preferences.toolArgs[t] ?? [])]),
    ) as Record<Tool, string>,
  );

  function handleTheme(t: Theme) {
    setTheme(t);
    apply(t);
  }

  function handleSave() {
    commitName();
    for (const tool of CONFIGURABLE_TOOLS) {
      commitArgs(tool);
    }
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

  function commitArgs(tool: Tool) {
    const parsed = textToArgs(argsText[tool] ?? '');
    const current = preferences.toolArgs[tool] ?? [];
    // Avoid a network roundtrip when nothing actually changed (e.g. user
    // tabs through the input without editing).
    if (parsed.length === current.length && parsed.every((v, i) => v === current[i])) {
      return;
    }
    onSavePreferences({
      ...preferences,
      toolArgs: { ...preferences.toolArgs, [tool]: parsed },
    });
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40">
      <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-xl p-6 w-full max-w-md mx-4">
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
          <p className="text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-2 uppercase tracking-wide">
            Default args per tool
          </p>
          <p className="text-xs text-zinc-500 dark:text-zinc-400 mb-3 leading-snug">
            Appended whenever you launch the tool from this account.
            Whitespace-separated; quoted args aren&apos;t supported.
          </p>
          <div className="space-y-2">
            {CONFIGURABLE_TOOLS.map((tool) => (
              <label key={tool} className="flex items-center gap-3">
                <span className="w-16 shrink-0 text-xs font-mono text-zinc-700 dark:text-zinc-300 capitalize">
                  {tool}
                </span>
                <input
                  type="text"
                  spellCheck={false}
                  autoCapitalize="off"
                  autoCorrect="off"
                  placeholder="--flag value …"
                  value={argsText[tool] ?? ''}
                  onChange={(e) =>
                    setArgsText((prev) => ({ ...prev, [tool]: e.target.value }))
                  }
                  className="flex-1 px-2 py-1.5 text-xs font-mono rounded-md border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-950 text-zinc-900 dark:text-zinc-100 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:focus:ring-zinc-500"
                />
              </label>
            ))}
          </div>
        </div>

        <div className="flex justify-end gap-2">
          <button
            onClick={handleClose}
            className="text-sm px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
          >
            Cancel
          </button>
          <button
            onClick={handleSave}
            className="text-sm px-3 py-1.5 rounded-lg bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90 transition-colors"
          >
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
