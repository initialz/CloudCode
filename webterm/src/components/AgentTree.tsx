// Left sidebar tree: agents -> workspaces.
// Expanded agents fetch workspaces via the control WS (handled by parent).

import { useState } from 'react';
import type { AgentItem, WorkspaceItem } from '@/lib/wire';

type WorkspaceState =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'loaded'; items: WorkspaceItem[] };

export type AgentWorkspaceCache = Map<string, WorkspaceState>;

type Props = {
  agents: AgentItem[];
  loading: boolean;
  cache: AgentWorkspaceCache;
  /** "agent::workspace" keys that already have a tab — used so a
   *  second click switches tabs instead of opening a duplicate. */
  openTabKeys: Set<string>;
  /** Key of the workspace whose tab is currently in focus (right pane).
   *  Only this row gets the selected-row background. */
  activeTabKey: string | null;
  onExpandAgent: (agent: string) => void;
  onOpenWorkspace: (agent: string, workspace: string) => void;
  onResetWorkspace: (agent: string, workspace: string) => void;
  onDeleteWorkspace: (agent: string, workspace: string) => void;
};

export default function AgentTree({
  agents,
  loading,
  cache,
  openTabKeys,
  activeTabKey,
  onExpandAgent,
  onOpenWorkspace,
  onResetWorkspace,
  onDeleteWorkspace,
}: Props) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  function toggleAgent(name: string, online: boolean) {
    if (!online) return;
    const next = new Set(expanded);
    if (next.has(name)) {
      next.delete(name);
    } else {
      next.add(name);
      onExpandAgent(name);
    }
    setExpanded(next);
  }

  if (loading) {
    return (
      <div className="px-3 py-2 text-xs text-zinc-400 dark:text-zinc-500">
        Loading agents...
      </div>
    );
  }

  if (agents.length === 0) {
    return (
      <div className="px-3 py-2 text-xs text-zinc-400 dark:text-zinc-500">
        No agents available.
      </div>
    );
  }

  return (
    <div className="flex-1 overflow-y-auto">
      {agents.map((agent) => {
        const isExpanded = expanded.has(agent.name);
        const wsState: WorkspaceState = cache.get(agent.name) ?? { status: 'idle' };

        return (
          <div key={agent.name}>
            {/* Agent row */}
            <button
              type="button"
              onClick={() => toggleAgent(agent.name, agent.current !== undefined ? true : true)}
              className={`w-full flex items-center gap-1.5 px-2 py-1 text-left text-xs font-mono transition-colors ${
                agent.current === false
                  ? 'text-zinc-400 dark:text-zinc-600 cursor-default'
                  : 'text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800'
              }`}
              aria-expanded={isExpanded}
            >
              {/* Chevron */}
              <span
                className={`shrink-0 transition-transform duration-150 ${isExpanded ? 'rotate-90' : ''}`}
              >
                <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
                  <path d="M3 2L7 5L3 8" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                </svg>
              </span>
              <span className="flex-1 truncate font-semibold">{agent.name}</span>
            </button>

            {/* Workspace list */}
            {isExpanded && (
              <div>
                {wsState.status === 'loading' && (
                  <div className="pl-7 pr-2 py-0.5 text-xs text-zinc-400 dark:text-zinc-500">
                    ...
                  </div>
                )}
                {wsState.status === 'loaded' && wsState.items.length === 0 && (
                  <div className="pl-7 pr-2 py-0.5 text-xs text-zinc-400 dark:text-zinc-500 italic">
                    no workspaces
                  </div>
                )}
                {wsState.status === 'loaded' &&
                  wsState.items.map((ws) => {
                    const key = `${agent.name}::${ws.name}`;
                    return (
                      <WorkspaceRow
                        key={ws.name}
                        workspace={ws}
                        isLive={openTabKeys.has(key)}
                        isActive={activeTabKey === key}
                        onOpen={() => onOpenWorkspace(agent.name, ws.name)}
                        onReset={() => onResetWorkspace(agent.name, ws.name)}
                        onDelete={() => onDeleteWorkspace(agent.name, ws.name)}
                      />
                    );
                  })}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

// ── WorkspaceRow ─────────────────────────────────────────────────────────────

function WorkspaceBadge({ ws, isLive }: { ws: WorkspaceItem; isLive: boolean }) {
  // Live > active (tracked by an open tab in this UI) takes priority
  // over hub-reported has_client, so the dot turns green the moment
  // you click open even before the hub's workspace_list refresh
  // arrives.
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
  isLive,
  isActive,
  onOpen,
  onReset,
  onDelete,
}: {
  workspace: WorkspaceItem;
  /** This workspace has an open tab somewhere (= "live"). */
  isLive: boolean;
  /** This workspace's tab is the one currently in the right pane. */
  isActive: boolean;
  onOpen: () => void;
  onReset: () => void;
  onDelete: () => void;
}) {
  return (
    <div
      className={`group flex items-center gap-1 pl-6 pr-1.5 py-0.5 text-xs font-mono cursor-pointer transition-colors ${
        isActive
          ? 'bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100'
          : 'text-zinc-600 dark:text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-800 hover:text-zinc-900 dark:hover:text-zinc-100'
      }`}
      onClick={onOpen}
    >
      {/* Status badge — fixed width for alignment */}
      <span className="w-3 text-center shrink-0">
        <WorkspaceBadge ws={workspace} isLive={isLive} />
      </span>
      <span className="flex-1 truncate">{workspace.name}</span>

      {/* Action buttons — hover-visible */}
      <span className="shrink-0 flex gap-0.5 opacity-0 group-hover:opacity-100 transition-opacity">
        <button
          onClick={(e) => {
            e.stopPropagation();
            onReset();
          }}
          className="p-0.5 rounded text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 hover:bg-zinc-200 dark:hover:bg-zinc-700 transition-colors"
          title={`Reset ${workspace.name}`}
          aria-label={`Reset workspace ${workspace.name}`}
        >
          <ResetIcon />
        </button>
        <button
          onClick={(e) => {
            e.stopPropagation();
            onDelete();
          }}
          className="p-0.5 rounded text-zinc-400 hover:text-red-600 dark:hover:text-red-400 hover:bg-red-50 dark:hover:bg-red-950/40 transition-colors"
          title={`Delete ${workspace.name}`}
          aria-label={`Delete workspace ${workspace.name}`}
        >
          <TrashIcon />
        </button>
      </span>
    </div>
  );
}

// ── Icons ────────────────────────────────────────────────────────────────────
// Inline so they inherit currentColor and don't need a separate
// icon-pack dependency. Sized to sit on a one-line tree row.

function ResetIcon() {
  // Lucide-style rotate-ccw: the arc has a real gap before its ends,
  // and the arrow sits in that gap pointing back into the loop, so
  // the head and tail aren't crammed together the way they were on
  // the previous open-ended arc.
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
