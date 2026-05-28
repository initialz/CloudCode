// Flat workspace list — one row per workspace, sorted online-first then
// agent↑ name↑. When two workspaces share a name across agents the display
// label becomes "name@agent" (matches cloudcode CLI menu.rs convention).

import { useState, useEffect, useMemo } from 'react';
import type { WorkspaceItem } from '@/lib/wire';

type Props = {
  workspaces: WorkspaceItem[];
  loading: boolean;
  /** "agent::workspace" keys that already have a tab. */
  openTabKeys: Set<string>;
  /** Key of the workspace whose tab is currently in focus. */
  activeTabKey: string | null;
  onOpenWorkspace: (agent: string, workspace: string, tool?: string) => void;
  onResetWorkspace: (agent: string, workspace: string) => void;
  onDeleteWorkspace: (agent: string, workspace: string) => void;
  onOpenFiles?: (agent: string, workspace: string) => void;
};

type WorkspaceMenu = { x: number; y: number; agent: string; workspace: string };

export default function AgentTree({
  workspaces,
  loading,
  openTabKeys,
  activeTabKey,
  onOpenWorkspace,
  onResetWorkspace,
  onDeleteWorkspace,
  onOpenFiles,
}: Props) {
  const [wsMenu, setWsMenu] = useState<WorkspaceMenu | null>(null);

  // Close menu on Escape.
  useEffect(() => {
    if (!wsMenu) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setWsMenu(null);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [wsMenu]);

  // Sorted list: online first, then by agent asc, then by name asc.
  const sorted = useMemo(() => {
    return [...workspaces].sort((a, b) => {
      const onlineDiff =
        (b.agent_online ? 1 : 0) - (a.agent_online ? 1 : 0);
      if (onlineDiff !== 0) return onlineDiff;
      if (a.agent !== b.agent) return a.agent.localeCompare(b.agent);
      return a.name.localeCompare(b.name);
    });
  }, [workspaces]);

  if (loading) {
    return (
      <div className="px-3 py-2 text-xs text-zinc-400 dark:text-zinc-500">
        Loading agents...
      </div>
    );
  }

  if (sorted.length === 0) {
    return (
      <div className="px-3 py-2 text-xs text-zinc-400 dark:text-zinc-500 italic">
        No workspaces yet.
      </div>
    );
  }

  return (
    <div className="flex-1 overflow-y-auto relative">
      {/* Global backdrop */}
      {wsMenu && (
        <div
          className="fixed inset-0 z-40"
          onClick={() => setWsMenu(null)}
          onContextMenu={(e) => { e.preventDefault(); setWsMenu(null); }}
        />
      )}

      {/* Workspace right-click context menu */}
      {wsMenu && (() => {
        const item = workspaces.find(
          (w) => w.agent === wsMenu.agent && w.name === wsMenu.workspace,
        );
        const isOnline = item?.agent_online === true;
        return (
          <div
            className="fixed z-50 min-w-[11rem] bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-700 rounded-md shadow-lg py-1 text-xs font-mono"
            style={{ left: wsMenu.x, top: wsMenu.y }}
          >
            <button
              type="button"
              onClick={() => {
                const { agent, workspace } = wsMenu;
                setWsMenu(null);
                onOpenWorkspace(agent, workspace);
              }}
              className="block w-full text-left px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
            >
              Open
            </button>
            {onOpenFiles && (
              <button
                type="button"
                onClick={() => {
                  const { agent, workspace } = wsMenu;
                  setWsMenu(null);
                  onOpenFiles(agent, workspace);
                }}
                className="w-full flex items-center gap-2 px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
              >
                <FolderOpenIcon />
                <span>Files</span>
              </button>
            )}
            <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
            <button
              type="button"
              onClick={() => {
                const { agent, workspace } = wsMenu;
                setWsMenu(null);
                if (!isOnline) return;
                onResetWorkspace(agent, workspace);
              }}
              disabled={!isOnline}
              className={`w-full flex items-center gap-2 px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 ${
                isOnline
                  ? 'text-zinc-700 dark:text-zinc-200'
                  : 'text-zinc-400 dark:text-zinc-600 cursor-not-allowed'
              }`}
            >
              <ResetIcon />
              <span>Reset</span>
            </button>
            <button
              type="button"
              onClick={() => {
                const { agent, workspace } = wsMenu;
                setWsMenu(null);
                onDeleteWorkspace(agent, workspace);
              }}
              className="w-full flex items-center gap-2 px-3 py-1.5 hover:bg-red-50 dark:hover:bg-red-950/30 text-red-600 dark:text-red-400"
            >
              <TrashIcon />
              <span>Delete</span>
            </button>
          </div>
        );
      })()}

      {sorted.map((ws) => {
        const label = `${ws.name}@${ws.agent}`;
        const key = `${ws.agent}::${ws.name}`;
        const isLive = openTabKeys.has(key);
        const isActive = activeTabKey === key;

        return (
          <WorkspaceRow
            key={key}
            workspace={ws}
            label={label}
            isLive={isLive}
            isActive={isActive}
            onOpen={() => {
              if (!ws.agent_online) return;
              onOpenWorkspace(ws.agent, ws.name);
            }}
            onContextMenu={(x, y) => {
              setWsMenu({ x, y, agent: ws.agent, workspace: ws.name });
            }}
          />
        );
      })}
    </div>
  );
}

// ── WorkspaceRow ─────────────────────────────────────────────────────────────

function WorkspaceBadge({ ws, isLive }: { ws: WorkspaceItem; isLive: boolean }) {
  if (isLive || ws.has_client) {
    return (
      <span className="text-emerald-500 font-bold" title="live">
        ●
      </span>
    );
  }
  if (ws.tmux_alive) {
    return (
      <span className="text-amber-500" title="saved">
        ·
      </span>
    );
  }
  return (
    <span className="text-transparent select-none" aria-hidden>
      ·
    </span>
  );
}

function WorkspaceRow({
  workspace,
  label,
  isLive,
  isActive,
  onOpen,
  onContextMenu,
}: {
  workspace: WorkspaceItem;
  label: string;
  isLive: boolean;
  isActive: boolean;
  onOpen: () => void;
  onContextMenu: (x: number, y: number) => void;
}) {
  const offline = !workspace.agent_online;
  const tooltip = offline
    ? `${label} — agent '${workspace.agent}' is offline`
    : label;

  return (
    <div
      className={`flex items-center gap-1.5 px-3 py-1.5 text-xs font-mono transition-colors ${
        offline
          ? 'text-zinc-400 dark:text-zinc-600 cursor-not-allowed'
          : isActive
            ? 'bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100 cursor-pointer'
            : 'text-zinc-600 dark:text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-800 hover:text-zinc-900 dark:hover:text-zinc-100 cursor-pointer'
      }`}
      onClick={onOpen}
      title={tooltip}
      onContextMenu={(e) => {
        e.preventDefault();
        onContextMenu(e.clientX, e.clientY);
      }}
    >
      <span className="w-3 text-center shrink-0">
        <WorkspaceBadge ws={workspace} isLive={isLive} />
      </span>
      <span className="flex-1 truncate">{label}</span>
    </div>
  );
}

// ── Icons ────────────────────────────────────────────────────────────────────


function FolderOpenIcon() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
      <polyline points="2 10 22 10" />
    </svg>
  );
}

function ResetIcon() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
      <path d="M3 3v5h5" />
    </svg>
  );
}

function TrashIcon() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d="M3 6h18" />
      <path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
      <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
      <path d="M10 11v6" />
      <path d="M14 11v6" />
    </svg>
  );
}
