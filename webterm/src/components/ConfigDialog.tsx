// Per-workspace config dialog. Scoped to a single (agent, workspace).
// Reuses the same env + args editors as the global SettingsDialog.
//
// Inheritance model: a workspace inherits the global config live until
// the user edits it; the first edit forks a snapshot of the effective
// global into preferences.workspaces[key], after which the workspace is
// independent. See preferences.ts (forkWorkspace / effectiveConfig).

import { useState } from 'react';
import { type Tool } from '@/lib/tools';
import {
  effectiveConfig,
  forkWorkspace,
  isForked,
  resetWorkspaceConfig,
  type Preferences,
} from '@/lib/preferences';
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
  agent: string;
  workspace: string;
  preferences: Preferences;
  onSavePreferences: (next: Preferences) => void;
  /** Reuses the existing reset path to relaunch with new env/args. */
  onRestartWorkspace: (agent: string, workspace: string) => void;
  onClose: () => void;
};

export default function ConfigDialog({
  agent,
  workspace,
  preferences,
  onSavePreferences,
  onRestartWorkspace,
  onClose,
}: Props) {
  const forked = isForked(preferences, agent, workspace);
  const cfg = effectiveConfig(preferences, agent, workspace);

  const [envRows, setEnvRows] = useState<EnvRow[]>(() => envToRows(cfg.env));
  const [argsText, setArgsText] = useState<Record<Tool, string>>(() =>
    initialArgsText(cfg),
  );
  const [saved, setSaved] = useState(false);

  const hasInvalidEnv = invalidRowIds(envRows).size > 0;

  function build(): Preferences {
    // First edit forks the effective global into a workspace snapshot,
    // then we overwrite that snapshot with the form's contents.
    const base = effectiveConfig(preferences, agent, workspace);
    const forkedPrefs = forkWorkspace(preferences, agent, workspace);
    const key = `${agent}/${workspace}`;
    return {
      ...forkedPrefs,
      workspaces: {
        ...forkedPrefs.workspaces,
        [key]: collapseConfig(base, envRows, argsText),
      },
    };
  }

  function persist(): Preferences {
    const next = build();
    onSavePreferences(next);
    return next;
  }

  function handleSave() {
    if (hasInvalidEnv) return;
    persist();
    setSaved(true);
  }

  function handleRestart() {
    if (hasInvalidEnv) return;
    persist();
    onRestartWorkspace(agent, workspace);
    onClose();
  }

  function handleResetToGlobal() {
    const next = resetWorkspaceConfig(preferences, agent, workspace);
    onSavePreferences(next);
    onClose();
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40">
      <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-xl p-6 w-full max-w-md mx-4 max-h-[90vh] overflow-y-auto">
        <h3 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 mb-1">
          Config: {workspace}
          <span className="ml-1 text-xs font-normal text-zinc-400 dark:text-zinc-500">
            @{agent}
          </span>
        </h3>
        <p className="text-xs text-zinc-500 dark:text-zinc-400 mb-4 leading-snug">
          {forked
            ? 'This workspace has its own config, independent of global.'
            : 'Inherits global until you edit, then becomes independent.'}
        </p>

        <div className="mb-5">
          <ArgsEditor
            argsText={argsText}
            onChange={(tool, text) => {
              setSaved(false);
              setArgsText((prev) => ({ ...prev, [tool]: text }));
            }}
          />
        </div>

        <div className="mb-5">
          <EnvEditor
            rows={envRows}
            onChange={(rows) => {
              setSaved(false);
              setEnvRows(rows);
            }}
          />
        </div>

        {saved && (
          <div className="mb-4 rounded-md border-l-2 border-emerald-500 bg-emerald-50 dark:bg-emerald-950/30 px-3 py-2 text-xs text-emerald-700 dark:text-emerald-300">
            Saved — applies on next launch. Use &ldquo;Restart workspace to
            apply now&rdquo; to relaunch immediately (your conversation is
            preserved).
          </div>
        )}

        <div className="flex flex-wrap items-center gap-2">
          {forked && (
            <button
              type="button"
              onClick={handleResetToGlobal}
              className="text-xs text-zinc-500 dark:text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100 underline transition-colors"
            >
              Reset to global
            </button>
          )}
          <div className="flex-1" />
          <button
            onClick={onClose}
            className="text-sm px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
          >
            Close
          </button>
          <button
            onClick={handleSave}
            disabled={hasInvalidEnv}
            className="text-sm px-3 py-1.5 rounded-lg bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            Save
          </button>
          <button
            onClick={handleRestart}
            disabled={hasInvalidEnv}
            title="Saves, then restarts this workspace so the new env/args take effect now. Conversation is preserved (--continue)."
            className="text-sm px-3 py-1.5 rounded-lg border border-zinc-300 dark:border-zinc-600 text-zinc-700 dark:text-zinc-200 hover:bg-zinc-50 dark:hover:bg-zinc-800 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            Restart workspace to apply now
          </button>
        </div>
      </div>
    </div>
  );
}
