// Left sidebar tree: agents -> workspaces.
// Expanded agents fetch workspaces via the control WS (handled by parent).

import { useState, useEffect } from 'react';
import type { AgentItem, WorkspaceItem } from '@/lib/wire';
import { toolsForAgent } from '@/lib/tools';
import type { Tool } from '@/lib/tools';

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
  onOpenWorkspace: (agent: string, workspace: string, tool?: string) => void;
  onResetWorkspace: (agent: string, workspace: string) => void;
  onDeleteWorkspace: (agent: string, workspace: string) => void;
  /** Triggered by the right-click context menu on an agent row,
   *  pre-filling the "create workspace" dialog with this agent. */
  onCreateWorkspaceFor: (agent: string) => void;
};

type AgentMenu = { x: number; y: number; agent: string };
type WorkspaceMenu = { x: number; y: number; agent: string; workspace: string };

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
  onCreateWorkspaceFor,
}: Props) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [agentMenu, setAgentMenu] = useState<AgentMenu | null>(null);
  const [wsMenu, setWsMenu] = useState<WorkspaceMenu | null>(null);

  const hasAnyMenu = agentMenu !== null || wsMenu !== null;

  // Close menus on Escape.
  useEffect(() => {
    if (!hasAnyMenu) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setAgentMenu(null);
        setWsMenu(null);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [hasAnyMenu]);

  function closeAllMenus() {
    setAgentMenu(null);
    setWsMenu(null);
  }

  function toggleAgent(name: string) {
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
    <div className="flex-1 overflow-y-auto relative">
      {/* Global backdrop for all context menus */}
      {hasAnyMenu && (
        <div
          className="fixed inset-0 z-40"
          onClick={closeAllMenus}
          onContextMenu={(e) => { e.preventDefault(); closeAllMenus(); }}
        />
      )}

      {/* Agent right-click context menu */}
      {agentMenu && (
        <div
          className="fixed z-50 min-w-[10rem] bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-700 rounded-md shadow-lg py-1 text-xs font-mono"
          style={{ left: agentMenu.x, top: agentMenu.y }}
        >
          <button
            type="button"
            onClick={() => {
              const a = agentMenu.agent;
              closeAllMenus();
              onCreateWorkspaceFor(a);
            }}
            className="block w-full text-left px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
          >
            Create workspace…
          </button>
        </div>
      )}

      {/* Workspace right-click context menu */}
      {wsMenu && (() => {
        const wsMenuAgent = agents.find((a) => a.name === wsMenu.agent);
        const wsMenuTools = toolsForAgent(wsMenuAgent?.tools);
        const wsMenuFirstTool: Tool | undefined = wsMenuTools[0];
        return (
          <div
            className="fixed z-50 min-w-[10rem] bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-700 rounded-md shadow-lg py-1 text-xs font-mono"
            style={{ left: wsMenu.x, top: wsMenu.y }}
          >
            {wsMenuTools.length > 0 ? (
              wsMenuTools.map((tool) => (
                <button
                  key={tool}
                  type="button"
                  onClick={() => {
                    const { agent, workspace } = wsMenu;
                    closeAllMenus();
                    onOpenWorkspace(agent, workspace, tool);
                  }}
                  className="block w-full text-left px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
                >
                  Open with {tool}
                </button>
              ))
            ) : (
              <button
                type="button"
                onClick={() => {
                  const { agent, workspace } = wsMenu;
                  closeAllMenus();
                  onOpenWorkspace(agent, workspace);
                }}
                className="block w-full text-left px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
                title={wsMenuFirstTool ? `Will run ${wsMenuFirstTool}` : undefined}
              >
                Open
              </button>
            )}
            <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
            <button
              type="button"
              onClick={() => {
                const { agent, workspace } = wsMenu;
                closeAllMenus();
                onResetWorkspace(agent, workspace);
              }}
              className="block w-full text-left px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
            >
              Reset
            </button>
            <button
              type="button"
              onClick={() => {
                const { agent, workspace } = wsMenu;
                closeAllMenus();
                onDeleteWorkspace(agent, workspace);
              }}
              className="block w-full text-left px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-red-600 dark:text-red-400"
            >
              Delete
            </button>
          </div>
        );
      })()}

      {agents.map((agent) => {
        const isExpanded = expanded.has(agent.name);
        const wsState: WorkspaceState = cache.get(agent.name) ?? { status: 'idle' };

        return (
          <div key={agent.name}>
            {/* Agent row */}
            <button
              type="button"
              onClick={() => toggleAgent(agent.name)}
              onContextMenu={(e) => {
                e.preventDefault();
                closeAllMenus();
                setAgentMenu({ x: e.clientX, y: e.clientY, agent: agent.name });
              }}
              className={`w-full flex items-center gap-1.5 px-2 py-1 text-left text-xs font-mono transition-colors ${
                agent.current === false
                  ? 'text-zinc-400 dark:text-zinc-600 cursor-default'
                  : agentMenu?.agent === agent.name
                    ? 'bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100'
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
                    const agentTools = toolsForAgent(agent.tools);
                    return (
                      <WorkspaceRow
                        key={ws.name}
                        workspace={ws}
                        agentTools={agentTools}
                        isLive={openTabKeys.has(key)}
                        isActive={activeTabKey === key}
                        onOpen={() => onOpenWorkspace(agent.name, ws.name)}
                        onOpenWithTool={(tool) => onOpenWorkspace(agent.name, ws.name, tool)}
                        onReset={() => onResetWorkspace(agent.name, ws.name)}
                        onDelete={() => onDeleteWorkspace(agent.name, ws.name)}
                        onContextMenu={(x, y) => {
                          closeAllMenus();
                          setWsMenu({ x, y, agent: agent.name, workspace: ws.name });
                        }}
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
  agentTools,
  isLive,
  isActive,
  onOpen,
  onOpenWithTool,
  onReset,
  onDelete,
  onContextMenu,
}: {
  workspace: WorkspaceItem;
  /** Tools available on the agent that owns this workspace. */
  agentTools: Tool[];
  /** This workspace has an open tab somewhere (= "live"). */
  isLive: boolean;
  /** This workspace's tab is the one currently in the right pane. */
  isActive: boolean;
  onOpen: () => void;
  onOpenWithTool: (tool: string) => void;
  onReset: () => void;
  onDelete: () => void;
  onContextMenu: (x: number, y: number) => void;
}) {
  const [dropdownOpen, setDropdownOpen] = useState(false);
  const [dropdownPos, setDropdownPos] = useState<{ x: number; y: number } | null>(null);

  // Only show the tool-picker chevron when there are 2+ tools to choose from.
  // With a single tool (or zero, which falls back to the first KNOWN_TOOL) the
  // "Open" click already does the right thing — no picker needed.
  const showChevron = agentTools.length > 1;

  function handleChevronClick(e: React.MouseEvent) {
    e.stopPropagation();
    if (dropdownOpen) {
      setDropdownOpen(false);
      setDropdownPos(null);
    } else {
      setDropdownPos({ x: e.clientX, y: e.clientY });
      setDropdownOpen(true);
    }
  }

  return (
    <>
      {/* Inline dropdown (hover chevron) — only rendered when multiple tools */}
      {showChevron && dropdownOpen && dropdownPos && (
        <>
          <div
            className="fixed inset-0 z-40"
            onClick={() => { setDropdownOpen(false); setDropdownPos(null); }}
            onContextMenu={(e) => { e.preventDefault(); setDropdownOpen(false); setDropdownPos(null); }}
          />
          <div
            className="fixed z-50 min-w-[10rem] bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-700 rounded-md shadow-lg py-1 text-xs font-mono"
            style={{ left: dropdownPos.x, top: dropdownPos.y }}
          >
            {agentTools.map((tool) => (
              <button
                key={tool}
                type="button"
                onClick={() => {
                  setDropdownOpen(false);
                  setDropdownPos(null);
                  onOpenWithTool(tool);
                }}
                className="block w-full text-left px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
              >
                Open with {tool}
              </button>
            ))}
          </div>
        </>
      )}

      <div
        className={`group flex items-center gap-1 pl-6 pr-1.5 py-0.5 text-xs font-mono cursor-pointer transition-colors ${
          isActive
            ? 'bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100'
            : 'text-zinc-600 dark:text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-800 hover:text-zinc-900 dark:hover:text-zinc-100'
        }`}
        onClick={onOpen}
        onContextMenu={(e) => {
          e.preventDefault();
          onContextMenu(e.clientX, e.clientY);
        }}
      >
        {/* Status badge — fixed width for alignment */}
        <span className="w-3 text-center shrink-0">
          <WorkspaceBadge ws={workspace} isLive={isLive} />
        </span>
        <span
          className="flex-1 truncate"
          title={agentTools.length === 1 ? `Will run ${agentTools[0]}` : undefined}
        >
          {workspace.name}
        </span>

        {/* Action buttons — hover-visible */}
        <span className="shrink-0 flex gap-0.5 opacity-0 group-hover:opacity-100 transition-opacity">
          {/* Tool selector chevron — only shown when multiple tools are available */}
          {showChevron && (
            <button
              onClick={handleChevronClick}
              className="p-0.5 rounded text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 hover:bg-zinc-200 dark:hover:bg-zinc-700 transition-colors"
              title="Open with tool..."
              aria-label={`Open ${workspace.name} with specific tool`}
            >
              <ChevronDownIcon />
            </button>
          )}
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
    </>
  );
}

// ── Icons ────────────────────────────────────────────────────────────────────
// Inline so they inherit currentColor and don't need a separate
// icon-pack dependency. Sized to sit on a one-line tree row.

function ChevronDownIcon() {
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
      <path d="M6 9l6 6 6-6" />
    </svg>
  );
}

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
