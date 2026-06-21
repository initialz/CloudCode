// Left sidebar: flat workspace list + "+ New workspace" button + account footer.

import { useState } from 'react';
import Logo from '@/components/Logo';
import AgentTree from '@/components/AgentTree';
import ConfirmDialog from '@/components/ConfirmDialog';
import type { AgentItem, WorkspaceItem } from '@/lib/wire';
import type { Preferences } from '@/lib/preferences';

type Props = {
  account: string;
  realName?: string | null;
  agents: AgentItem[];
  agentsLoading: boolean;
  workspaces: WorkspaceItem[];
  openTabKeys: Set<string>;
  activeTabKey: string | null;
  preferences: Preferences;
  onSavePreferences: (next: Preferences) => void;
  onOpenWorkspace: (agent: string, workspace: string, tool?: string) => void;
  onCreateWorkspace: (agent: string, name: string) => void;
  onResetWorkspace: (agent: string, workspace: string) => void;
  onDeleteWorkspace: (agent: string, workspace: string) => void;
  onOpenFiles?: (agent: string, workspace: string) => void;
  onConfigWorkspace?: (agent: string, workspace: string) => void;
  onSettings: () => void;
  onLogout: () => void;
};

type ConfirmState = {
  title: string;
  body: string;
  label: string;
  danger: boolean;
  onConfirm: () => void;
};

export default function Sidebar({
  account,
  realName,
  agents,
  agentsLoading,
  workspaces,
  openTabKeys,
  activeTabKey,
  preferences,
  onSavePreferences,
  onOpenWorkspace,
  onCreateWorkspace,
  onResetWorkspace,
  onDeleteWorkspace,
  onOpenFiles,
  onConfigWorkspace,
  onSettings,
  onLogout,
}: Props) {
  const [showCreate, setShowCreate] = useState(false);
  const [createAgent, setCreateAgent] = useState('');
  const [createName, setCreateName] = useState('');
  const [createError, setCreateError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<ConfirmState | null>(null);

  function openCreate() {
    // Default to the first available agent.
    if (agents.length > 0) setCreateAgent(agents[0].name);
    setCreateName('');
    setCreateError(null);
    setShowCreate(true);
  }

  function submitCreate() {
    const name = createName.trim();
    if (!name || !createAgent) return;
    // Same-agent uniqueness: catch the collision client-side so we
    // can keep the modal open with an inline error instead of letting
    // the hub bounce back a SessionError toast that closes the
    // dialog and forces a retry from scratch.
    const clash = workspaces.some(
      (w) => w.agent === createAgent && w.name === name,
    );
    if (clash) {
      setCreateError(
        `Workspace "${name}" already exists on agent "${createAgent}".`,
      );
      return;
    }
    onCreateWorkspace(createAgent, name);
    setShowCreate(false);
    setCreateName('');
    setCreateError(null);
  }

  function askReset(agent: string, workspace: string) {
    setConfirm({
      title: 'Reset workspace?',
      body: `This will reset "${workspace}" to a fresh state.`,
      label: 'Reset',
      danger: false,
      onConfirm: () => {
        setConfirm(null);
        onResetWorkspace(agent, workspace);
      },
    });
  }

  function askDelete(agent: string, workspace: string) {
    setConfirm({
      title: 'Delete workspace?',
      body: `This will permanently delete "${workspace}".`,
      label: 'Delete',
      danger: true,
      onConfirm: () => {
        setConfirm(null);
        onDeleteWorkspace(agent, workspace);
      },
    });
  }

  return (
    <>
      <aside className="flex flex-col w-60 shrink-0 border-r border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900 h-full overflow-hidden">
        {/* Header */}
        <div className="flex items-center gap-2 px-3 py-2.5 border-b border-zinc-200 dark:border-zinc-800 shrink-0">
          <Logo size={18} className="text-zinc-700 dark:text-zinc-300 shrink-0" />
          <span className="text-sm font-semibold text-zinc-800 dark:text-zinc-200 select-none">
            CloudCode
          </span>
        </div>

        {/* "+ New workspace" button — pinned below header */}
        <div className="shrink-0 px-3 pt-3 pb-2" data-tutorial="new-workspace">
          <button
            type="button"
            onClick={openCreate}
            disabled={agents.length === 0}
            className="w-full flex items-center justify-center gap-1.5 px-3 py-2 rounded-md text-xs font-mono font-medium text-zinc-600 dark:text-zinc-300 bg-white dark:bg-zinc-800 border border-zinc-200 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-700 hover:text-zinc-900 dark:hover:text-zinc-100 disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
          >
            <PlusIcon />
            New workspace
          </button>
        </div>

        {/* Flat workspace list */}
        <div className="flex-1 overflow-y-auto py-1" data-tutorial="workspace-list">
          <AgentTree
            workspaces={workspaces}
            loading={agentsLoading}
            openTabKeys={openTabKeys}
            activeTabKey={activeTabKey}
            preferences={preferences}
            onSavePreferences={onSavePreferences}
            onOpenWorkspace={onOpenWorkspace}
            onResetWorkspace={askReset}
            onDeleteWorkspace={askDelete}
            onOpenFiles={onOpenFiles}
            onConfigWorkspace={onConfigWorkspace}
          />
        </div>

        {/* Account / actions footer */}
        <div className="shrink-0 px-3 py-2.5 border-t border-zinc-200 dark:border-zinc-800">
          {realName ? (
            <div className="mb-1.5">
              <div className="text-xs font-medium text-zinc-700 dark:text-zinc-300 truncate">
                {realName}
              </div>
              <div className="text-[10px] text-zinc-400 dark:text-zinc-500 font-mono truncate">
                {account}
              </div>
            </div>
          ) : (
            <div className="text-xs text-zinc-600 dark:text-zinc-400 font-mono truncate mb-1.5">
              {account}
            </div>
          )}
          <div className="flex gap-2">
            <button
              data-tutorial="settings"
              onClick={onSettings}
              className="text-xs text-zinc-400 dark:text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300 transition-colors"
            >
              settings
            </button>
            <span className="text-zinc-300 dark:text-zinc-700 select-none">·</span>
            <button
              onClick={onLogout}
              className="text-xs text-zinc-400 dark:text-zinc-500 hover:text-red-600 dark:hover:text-red-400 transition-colors"
            >
              logout
            </button>
          </div>
        </div>
      </aside>

      {/* Create workspace modal */}
      {showCreate && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40">
          <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-xl p-6 w-full max-w-sm mx-4">
            <h3 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 mb-4">
              Create workspace
            </h3>

            <div className="mb-3">
              <label className="block text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-1 uppercase tracking-wide">
                Name
              </label>
              <input
                type="text"
                placeholder="workspace name"
                value={createName}
                onChange={(e) => {
                  setCreateName(e.target.value);
                  if (createError) setCreateError(null);
                }}
                onKeyDown={(e) => e.key === 'Enter' && submitCreate()}
                autoFocus
                className="w-full rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-800 px-3 py-2 text-sm text-zinc-900 dark:text-zinc-100 placeholder-zinc-400 focus:outline-none focus:ring-2 focus:ring-zinc-500 font-mono"
              />
            </div>

            <div className="mb-4">
              <label className="block text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-1 uppercase tracking-wide">
                Agent
              </label>
              <select
                value={createAgent}
                onChange={(e) => {
                  setCreateAgent(e.target.value);
                  if (createError) setCreateError(null);
                }}
                className="w-full rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-800 px-3 py-2 text-sm text-zinc-900 dark:text-zinc-100 focus:outline-none focus:ring-2 focus:ring-zinc-500 font-mono"
              >
                {agents.map((a) => (
                  <option key={a.name} value={a.name}>
                    {a.name}
                  </option>
                ))}
              </select>
            </div>

            {createError && (
              <div className="mb-3 rounded-md border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-xs text-red-700 dark:text-red-300">
                {createError}
              </div>
            )}

            <div className="flex gap-2 justify-end">
              <button
                onClick={() => setShowCreate(false)}
                className="text-sm px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
              >
                Cancel
              </button>
              <button
                onClick={submitCreate}
                disabled={!createName.trim() || !createAgent}
                className="text-sm px-3 py-1.5 rounded-lg bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:bg-zinc-700 dark:hover:bg-zinc-300 disabled:opacity-50 transition-colors"
              >
                Create
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Confirm dialog */}
      {confirm && (
        <ConfirmDialog
          title={confirm.title}
          body={confirm.body}
          confirmLabel={confirm.label}
          danger={confirm.danger}
          onConfirm={confirm.onConfirm}
          onCancel={() => setConfirm(null)}
        />
      )}
    </>
  );
}

// ── Icons ────────────────────────────────────────────────────────────────────

function PlusIcon() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d="M12 5v14M5 12h14" />
    </svg>
  );
}
