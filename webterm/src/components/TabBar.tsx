// Horizontal tab bar for the workbench editor area.

import type { Tab } from '@/lib/tabs';
import { tabLabel } from '@/lib/tabs';

type Props = {
  tabs: Tab[];
  activeTabId: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
};

export default function TabBar({ tabs, activeTabId, onSelect, onClose }: Props) {
  if (tabs.length === 0) return null;

  return (
    <div className="flex items-end shrink-0 overflow-x-auto bg-zinc-100 dark:bg-zinc-900 border-b border-zinc-200 dark:border-zinc-800">
      {tabs.map((tab) => {
        const isActive = tab.id === activeTabId;
        return (
          <div
            key={tab.id}
            className={`group relative flex items-center gap-1.5 px-3 py-2 text-xs font-mono cursor-pointer select-none shrink-0 max-w-[200px] border-r border-zinc-200 dark:border-zinc-800 transition-colors ${
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

            {/* Close button — always visible on active, hover-visible on inactive */}
            <button
              className={`shrink-0 ml-1 rounded p-0.5 transition-colors ${
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
  );
}
