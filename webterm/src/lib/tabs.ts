// Tab type + helpers for the workbench multi-session model.

import type { Terminal } from '@xterm/xterm';
import type { FitAddon } from '@xterm/addon-fit';
import type { WireSocket } from './wire';

export type TabStatus =
  | 'connecting'
  | 'opening'
  | 'live'
  // WS dropped without a terminal `rejected` frame; the workbench is
  // looping a fresh WireSocket + open_session under the hood with
  // exponential backoff. The terminal stays mounted and visible so the
  // user keeps their scrollback context.
  | 'reconnecting'
  | 'closed'
  | 'error';

export type Tab = {
  id: string;
  agent: string;
  workspace: string;
  /** Tool used when opening the session (e.g. 'claude', 'codex'). */
  tool?: string;
  status: TabStatus;
  errorMsg?: string;
  ws: WireSocket;
  term: Terminal;
  fitAddon: FitAddon;
  /** Has term.open() been called for this tab yet? Mutated by the
   * container-attach ref callback so we don't re-attach on every
   * render and infinite-loop. */
  opened: boolean;
};

/** Stable key used to deduplicate tabs. */
export function tabKey(agent: string, workspace: string): string {
  return `${agent}::${workspace}`;
}

/** Human-readable label shown in the tab bar. */
export function tabLabel(tab: Pick<Tab, 'agent' | 'workspace' | 'tool'>): string {
  const base = `${tab.workspace}@${tab.agent}`;
  return tab.tool ? `${base}·${tab.tool}` : base;
}
