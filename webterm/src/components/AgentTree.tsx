// Flat workspace list — one row per workspace, sorted online-first then
// agent↑ name↑. When two workspaces share a name across agents the display
// label becomes "name@agent" (matches cloudcode CLI menu.rs convention).

import { useState, useEffect, useMemo, type MouseEvent } from 'react';
import type { WorkspaceItem } from '@/lib/wire';
import {
  type Preferences,
  sortByPreference,
  isPinned,
  togglePin,
  moveWorkspace,
} from '@/lib/preferences';

type Props = {
  workspaces: WorkspaceItem[];
  loading: boolean;
  /** "agent::workspace" keys that already have a tab. */
  openTabKeys: Set<string>;
  /** Key of the workspace whose tab is currently in focus. */
  activeTabKey: string | null;
  preferences: Preferences;
  onSavePreferences: (next: Preferences) => void;
  onOpenWorkspace: (agent: string, workspace: string, tool?: string) => void;
  onResetWorkspace: (agent: string, workspace: string) => void;
  onDeleteWorkspace: (agent: string, workspace: string) => void;
  onOpenFiles?: (agent: string, workspace: string) => void;
  onConfigWorkspace?: (agent: string, workspace: string) => void;
};

type WorkspaceMenu = { x: number; y: number; agent: string; workspace: string };

export default function AgentTree({
  workspaces,
  loading,
  openTabKeys,
  activeTabKey,
  preferences,
  onSavePreferences,
  onOpenWorkspace,
  onResetWorkspace,
  onDeleteWorkspace,
  onOpenFiles,
  onConfigWorkspace,
}: Props) {
  const [wsMenu, setWsMenu] = useState<WorkspaceMenu | null>(null);

  const togglePinned = (agent: string, workspace: string) =>
    onSavePreferences(togglePin(preferences, workspaces, agent, workspace));
  const move = (agent: string, workspace: string, dir: 'up' | 'down') =>
    onSavePreferences(moveWorkspace(preferences, workspaces, agent, workspace, dir));

  // Close menu on Escape.
  useEffect(() => {
    if (!wsMenu) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setWsMenu(null);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [wsMenu]);

  // Sorted by the user's pin/rank preferences (pinned group first, each
  // group independently ordered), falling back to online-first / agent↑ /
  // name↑ for untouched workspaces.
  const sorted = useMemo(
    () => sortByPreference(preferences, workspaces),
    [preferences, workspaces],
  );
  const pinnedCount = useMemo(
    () => sorted.filter((w) => isPinned(preferences, w.agent, w.name)).length,
    [sorted, preferences],
  );

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
            {onConfigWorkspace && (
              <button
                type="button"
                onClick={() => {
                  const { agent, workspace } = wsMenu;
                  setWsMenu(null);
                  onConfigWorkspace(agent, workspace);
                }}
                className="w-full flex items-center gap-2 px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
              >
                <SlidersIcon />
                <span>Config</span>
              </button>
            )}
            {(onOpenFiles || onConfigWorkspace) && (
              <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
            )}
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

      {sorted.map((ws, i) => {
        const label = `${ws.name}@${ws.agent}`;
        const key = `${ws.agent}::${ws.name}`;
        const isLive = openTabKeys.has(key);
        const isActive = activeTabKey === key;
        const pinned = i < pinnedCount;
        // Position within this row's own group (pinned vs unpinned), used to
        // grey out the up/down arrows at the group edges.
        const idxInGroup = pinned ? i : i - pinnedCount;
        const groupLen = pinned ? pinnedCount : sorted.length - pinnedCount;
        // A faint separator between the pinned group and the rest.
        const divider = i === pinnedCount && pinnedCount > 0 && pinnedCount < sorted.length;

        return (
          <div key={key}>
            {divider && (
              <div className="mx-3 my-1 border-t border-dashed border-zinc-200 dark:border-zinc-700/70" />
            )}
            <WorkspaceRow
              workspace={ws}
              label={label}
              isLive={isLive}
              isActive={isActive}
              pinned={pinned}
              canUp={idxInGroup > 0}
              canDown={idxInGroup < groupLen - 1}
              onOpen={() => {
                if (!ws.agent_online) return;
                onOpenWorkspace(ws.agent, ws.name);
              }}
              onTogglePin={() => togglePinned(ws.agent, ws.name)}
              onMove={(dir) => move(ws.agent, ws.name, dir)}
              onContextMenu={(x, y) => {
                setWsMenu({ x, y, agent: ws.agent, workspace: ws.name });
              }}
            />
          </div>
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
  pinned,
  canUp,
  canDown,
  onOpen,
  onTogglePin,
  onMove,
  onContextMenu,
}: {
  workspace: WorkspaceItem;
  label: string;
  isLive: boolean;
  isActive: boolean;
  pinned: boolean;
  canUp: boolean;
  canDown: boolean;
  onOpen: () => void;
  onTogglePin: () => void;
  onMove: (dir: 'up' | 'down') => void;
  onContextMenu: (x: number, y: number) => void;
}) {
  const offline = !workspace.agent_online;
  const tooltip = offline
    ? `${label} — agent '${workspace.agent}' is offline`
    : label;

  // Reorder/pin controls must not also trigger the row's open-on-click.
  const stop = (fn: () => void) => (e: MouseEvent) => {
    e.stopPropagation();
    fn();
  };

  return (
    <div
      className={`group flex items-center gap-1.5 px-3 py-1.5 text-xs font-mono transition-colors ${
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

      {/* Reorder + pin controls. The up/down arrows show on hover; the star
          stays visible while pinned, otherwise reveals on hover. */}
      <span className="flex items-center gap-0.5 shrink-0">
        <button
          type="button"
          aria-label="Move up"
          title="Move up"
          disabled={!canUp}
          onClick={stop(() => onMove('up'))}
          className={`opacity-0 group-hover:opacity-100 transition-opacity p-0.5 ${
            canUp
              ? 'text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200'
              : 'text-zinc-300 dark:text-zinc-700 cursor-default'
          }`}
        >
          <ChevronUpIcon />
        </button>
        <button
          type="button"
          aria-label="Move down"
          title="Move down"
          disabled={!canDown}
          onClick={stop(() => onMove('down'))}
          className={`opacity-0 group-hover:opacity-100 transition-opacity p-0.5 ${
            canDown
              ? 'text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200'
              : 'text-zinc-300 dark:text-zinc-700 cursor-default'
          }`}
        >
          <ChevronDownIcon />
        </button>
        <button
          type="button"
          aria-label={pinned ? 'Unpin' : 'Pin to top'}
          title={pinned ? 'Unpin' : 'Pin to top'}
          onClick={stop(onTogglePin)}
          className={`p-0.5 transition-opacity ${
            pinned
              ? 'text-amber-500'
              : 'text-zinc-400 opacity-0 group-hover:opacity-100 hover:text-amber-500'
          }`}
        >
          <StarIcon filled={pinned} />
        </button>
      </span>
    </div>
  );
}

// ── Icons ────────────────────────────────────────────────────────────────────

function StarIcon({ filled }: { filled: boolean }) {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill={filled ? 'currentColor' : 'none'}
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2" />
    </svg>
  );
}

function ChevronUpIcon() {
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
      <polyline points="18 15 12 9 6 15" />
    </svg>
  );
}

function ChevronDownIcon() {
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
      <polyline points="6 9 12 15 18 9" />
    </svg>
  );
}

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

function SlidersIcon() {
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
      <line x1="4" y1="21" x2="4" y2="14" />
      <line x1="4" y1="10" x2="4" y2="3" />
      <line x1="12" y1="21" x2="12" y2="12" />
      <line x1="12" y1="8" x2="12" y2="3" />
      <line x1="20" y1="21" x2="20" y2="16" />
      <line x1="20" y1="12" x2="20" y2="3" />
      <line x1="1" y1="14" x2="7" y2="14" />
      <line x1="9" y1="8" x2="15" y2="8" />
      <line x1="17" y1="16" x2="23" y2="16" />
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
