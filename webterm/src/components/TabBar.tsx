// Horizontal tab bar for the workbench editor area.

import { useState } from 'react';
import type { Tab } from '@/lib/tabs';
import { tabLabel } from '@/lib/tabs';
import { KNOWN_TOOLS } from '@/lib/tools';
import type { SplitDirection } from '@/lib/wire';

type Props = {
  tabs: Tab[];
  activeTabId: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onSplit: (tabId: string, tool: string, direction: SplitDirection) => void;
};

const SPLIT_DIRECTIONS: { value: SplitDirection; label: string; arrow: string }[] = [
  { value: 'right', label: 'Right', arrow: '→' },
  { value: 'down', label: 'Down', arrow: '↓' },
];

export default function TabBar({
  tabs,
  activeTabId,
  onSelect,
  onClose,
  onSplit,
}: Props) {
  const [splitDropdownTabId, setSplitDropdownTabId] = useState<string | null>(null);
  const [splitDropdownPos, setSplitDropdownPos] = useState<{ x: number; y: number } | null>(null);
  const [hoveredTool, setHoveredTool] = useState<string | null>(null);

  if (tabs.length === 0) return null;

  function handleSplitClick(e: React.MouseEvent, tabId: string) {
    e.stopPropagation();
    if (splitDropdownTabId === tabId) {
      setSplitDropdownTabId(null);
      setSplitDropdownPos(null);
      setHoveredTool(null);
    } else {
      setSplitDropdownTabId(tabId);
      setSplitDropdownPos({ x: e.clientX, y: e.clientY });
      setHoveredTool(null);
    }
  }

  function closeDropdown() {
    setSplitDropdownTabId(null);
    setSplitDropdownPos(null);
    setHoveredTool(null);
  }

  return (
    <>
      {/* Split dropdown */}
      {splitDropdownTabId && splitDropdownPos && (
        <>
          <div
            className="fixed inset-0 z-40"
            onClick={closeDropdown}
            onContextMenu={(e) => { e.preventDefault(); closeDropdown(); }}
          />
          <div
            className="fixed z-50 min-w-[10rem] bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-700 rounded-md shadow-lg py-1 text-xs font-mono"
            style={{ left: splitDropdownPos.x, top: splitDropdownPos.y }}
            onMouseLeave={() => setHoveredTool(null)}
          >
            {KNOWN_TOOLS.map((tool) => (
              <div
                key={tool}
                className="relative"
                onMouseEnter={() => setHoveredTool(tool)}
              >
                <div
                  className={`flex items-center justify-between px-3 py-1.5 cursor-default text-zinc-700 dark:text-zinc-200 ${
                    hoveredTool === tool
                      ? 'bg-zinc-100 dark:bg-zinc-800'
                      : ''
                  }`}
                >
                  <span>Split with {tool}</span>
                  <span className="ml-3 text-zinc-400 dark:text-zinc-500">▸</span>
                </div>

                {/* Secondary flyout: direction picker. Rendered as a child
                    of the row so the mouse keeps hovering the parent. */}
                {hoveredTool === tool && (
                  <div className="absolute left-full top-0 -ml-px min-w-[7rem] bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-700 rounded-md shadow-lg py-1">
                    {SPLIT_DIRECTIONS.map(({ value, label, arrow }) => (
                      <button
                        key={value}
                        type="button"
                        onClick={() => {
                          const tabId = splitDropdownTabId;
                          closeDropdown();
                          onSplit(tabId, tool, value);
                        }}
                        className="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-200"
                      >
                        <span className="w-3 inline-block text-zinc-500 dark:text-zinc-400">{arrow}</span>
                        <span>{label}</span>
                      </button>
                    ))}
                  </div>
                )}
              </div>
            ))}
          </div>
        </>
      )}

      <div className="flex items-end shrink-0 overflow-x-auto bg-zinc-100 dark:bg-zinc-900 border-b border-zinc-200 dark:border-zinc-800">
        {tabs.map((tab) => {
          const isActive = tab.id === activeTabId;
          return (
            <div
              key={tab.id}
              className={`group relative flex items-center gap-1.5 px-3 py-2 text-xs font-mono cursor-pointer select-none shrink-0 max-w-[240px] border-r border-zinc-200 dark:border-zinc-800 transition-colors ${
                isActive
                  ? 'bg-white dark:bg-zinc-950 text-zinc-900 dark:text-zinc-100 border-b-2 border-b-zinc-900 dark:border-b-zinc-100 -mb-px'
                  : 'text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-800 hover:text-zinc-700 dark:hover:text-zinc-300'
              }`}
              onClick={() => onSelect(tab.id)}
            >
              {/* Status dot */}
              {tab.status === 'live' && (
                <span className="shrink-0 w-1.5 h-1.5 rounded-full bg-green-500" />
              )}
              {(tab.status === 'connecting' || tab.status === 'opening') && (
                <span className="shrink-0 w-1.5 h-1.5 rounded-full bg-yellow-500 animate-pulse" />
              )}
              {(tab.status === 'closed' || tab.status === 'error') && (
                <span className="shrink-0 w-1.5 h-1.5 rounded-full bg-red-500" />
              )}

              <span className="truncate">{tabLabel(tab)}</span>

              {/* Split + Layout buttons — only on active tab, left of close */}

              {/* Close button — always visible on active, hover-visible on inactive */}
              <button
                className={`shrink-0 ml-0.5 rounded p-0.5 transition-colors ${
                  isActive
                    ? 'opacity-60 hover:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-700'
                    : 'opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-zinc-300 dark:hover:bg-zinc-700'
                }`}
                onClick={(e) => {
                  e.stopPropagation();
                  onClose(tab.id);
                }}
                aria-label={`Close ${tabLabel(tab)}`}
              >
                <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
                  <path d="M2 2L8 8M8 2L2 8" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
                </svg>
              </button>
            </div>
          );
        })}
      </div>
    </>
  );
}
