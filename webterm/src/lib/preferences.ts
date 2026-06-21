// Per-user webterm preferences. The hub stores these as an opaque JSON
// blob keyed by account; webterm owns the schema and validates on read.
//
// Today this holds:
//   - default CLI args per tool (global)
//   - environment variables (global, shared across tools — NOT per-tool)
//   - per-workspace overrides (a forked snapshot of {env, toolArgs})
// Old rows without the new keys fall back to defaults via the parse step.

import { KNOWN_TOOLS, type Tool } from './tools';

/** Validation for an env var key. POSIX-ish identifier. */
export const ENV_KEY_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** A flat environment map shared across all tools at a given level. */
export type EnvMap = Record<string, string>;

/** Config that exists at both the global and per-workspace levels. */
export type ScopedConfig = {
  /** Environment variables (shared across tools). */
  env: EnvMap;
  /** Default argv appended to each tool when starting a session. */
  toolArgs: Record<Tool, string[]>;
};

/** Sort/pin state for one workspace, keyed by `<agent>/<workspace>`.
 *  `pinned` floats it into the pinned group; `rank` orders it within its
 *  group (pinned and unpinned are sorted independently — see
 *  `sortByPreference`). Workspaces without an entry fall back to the
 *  default online-first / agent↑ / name↑ order, after any tracked ones. */
export type WorkspaceOrderEntry = { pinned: boolean; rank: number };

export type Preferences = ScopedConfig & {
  /** Per-workspace forked snapshots, keyed by `<agent>/<workspace>`.
   *  A key present means the workspace has been customised (forked);
   *  a key absent means it inherits the global config live. */
  workspaces: Record<string, ScopedConfig>;
  /** User-defined sort + pin state, keyed by `<agent>/<workspace>`.
   *  Shared with the cloudcode CLI menu via the same hub prefs blob. */
  workspaceOrder: Record<string, WorkspaceOrderEntry>;
};

/** Minimal shape the ordering helpers need from a workspace row. */
export type Orderable = { agent: string; name: string; agent_online?: boolean };

function emptyToolArgs(): Record<Tool, string[]> {
  // Build with an explicit object so TS keeps the typed key shape; using
  // Object.fromEntries widens the keys back to `string` here.
  const out = {} as Record<Tool, string[]>;
  for (const t of KNOWN_TOOLS) out[t] = [];
  return out;
}

export const DEFAULT_PREFERENCES: Preferences = {
  env: {},
  toolArgs: emptyToolArgs(),
  workspaces: {},
  workspaceOrder: {},
};

/** The key under `workspaces` for a given (agent, workspace) pair. */
export function workspaceKey(agent: string, workspace: string): string {
  return `${agent}/${workspace}`;
}

// ── Wire shape ──────────────────────────────────────────────────────────────
// Stored on the hub as:
//   { tool_args: { claude: [...], codex: [...] },
//     env: { KEY: "VALUE" },
//     workspaces: { "<agent>/<ws>": { env, tool_args } } }
// Keep the wire keys snake_case to match the rest of the hub's JSON
// conventions; map to camelCase at the boundary.

type WireScoped = {
  tool_args?: Record<string, string[]>;
  env?: Record<string, string>;
};

type WireOrderEntry = { pinned?: boolean; rank?: number };

type WireShape = WireScoped & {
  workspaces?: Record<string, WireScoped>;
  workspace_order?: Record<string, WireOrderEntry>;
};

function parseEnv(raw: unknown): EnvMap {
  const out: EnvMap = {};
  if (!raw || typeof raw !== 'object') return out;
  for (const [k, v] of Object.entries(raw as Record<string, unknown>)) {
    if (typeof k === 'string' && typeof v === 'string') {
      out[k] = v;
    }
  }
  return out;
}

function parseToolArgs(raw: unknown): Record<Tool, string[]> {
  const out = emptyToolArgs();
  if (!raw || typeof raw !== 'object') return out;
  const map = raw as Record<string, unknown>;
  for (const tool of KNOWN_TOOLS) {
    const v = map[tool];
    if (Array.isArray(v) && v.every((x) => typeof x === 'string')) {
      out[tool] = v;
    }
  }
  return out;
}

function parseScoped(raw: WireScoped | undefined): ScopedConfig {
  return {
    env: parseEnv(raw?.env),
    toolArgs: parseToolArgs(raw?.tool_args),
  };
}

/** Parse a server-side blob into a typed Preferences, filling in any
 *  missing pieces with defaults. Never throws — bad data falls back. */
export function parsePreferences(blob: unknown): Preferences {
  if (!blob || typeof blob !== 'object') {
    return {
      env: {},
      toolArgs: emptyToolArgs(),
      workspaces: {},
      workspaceOrder: {},
    };
  }
  const wire = blob as WireShape;
  const base: Preferences = {
    ...parseScoped(wire),
    workspaces: {},
    workspaceOrder: {},
  };
  if (wire.workspaces && typeof wire.workspaces === 'object') {
    for (const [key, val] of Object.entries(wire.workspaces)) {
      if (typeof key !== 'string' || !key) continue;
      if (!val || typeof val !== 'object') continue;
      base.workspaces[key] = parseScoped(val as WireScoped);
    }
  }
  if (wire.workspace_order && typeof wire.workspace_order === 'object') {
    for (const [key, val] of Object.entries(wire.workspace_order)) {
      if (typeof key !== 'string' || !key) continue;
      if (!val || typeof val !== 'object') continue;
      const rank = typeof val.rank === 'number' && Number.isFinite(val.rank) ? val.rank : 0;
      base.workspaceOrder[key] = { pinned: val.pinned === true, rank };
    }
  }
  return base;
}

function serializeScoped(cfg: ScopedConfig): WireScoped {
  const toolArgs: Record<string, string[]> = {};
  for (const t of KNOWN_TOOLS) toolArgs[t] = cfg.toolArgs[t] ?? [];
  return { tool_args: toolArgs, env: { ...cfg.env } };
}

/** Reverse of parsePreferences — produce the wire blob for storage. */
export function serializePreferences(prefs: Preferences): WireShape {
  const out: WireShape = serializeScoped(prefs);
  const workspaces: Record<string, WireScoped> = {};
  for (const [key, cfg] of Object.entries(prefs.workspaces)) {
    workspaces[key] = serializeScoped(cfg);
  }
  out.workspaces = workspaces;
  const order: Record<string, WireOrderEntry> = {};
  for (const [key, entry] of Object.entries(prefs.workspaceOrder)) {
    order[key] = { pinned: entry.pinned, rank: entry.rank };
  }
  out.workspace_order = order;
  return out;
}

// ── Workspace fork / resolution helpers ──────────────────────────────────────

/** Deep-copy a scoped config so a forked workspace can be edited
 *  independently of the source it was copied from. */
function cloneScoped(cfg: ScopedConfig): ScopedConfig {
  const toolArgs = emptyToolArgs();
  for (const t of KNOWN_TOOLS) toolArgs[t] = [...(cfg.toolArgs[t] ?? [])];
  return {
    env: { ...cfg.env },
    toolArgs,
  };
}

/** The effective config for a workspace: its forked snapshot if it has
 *  one, otherwise the global config. Returns the actual reference, so
 *  callers must not mutate it in place — use `forkWorkspace` first. */
export function effectiveConfig(
  prefs: Preferences,
  agent: string,
  workspace: string,
): ScopedConfig {
  const key = workspaceKey(agent, workspace);
  return prefs.workspaces[key] ?? { env: prefs.env, toolArgs: prefs.toolArgs };
}

/** Whether a workspace has its own forked config (has been customised). */
export function isForked(
  prefs: Preferences,
  agent: string,
  workspace: string,
): boolean {
  return workspaceKey(agent, workspace) in prefs.workspaces;
}

/** Snapshot-fork: return a new Preferences whose `workspaces[key]` is a
 *  deep copy of the current effective global config. If the workspace
 *  already has a fork, the existing one is preserved unchanged. The
 *  copy is what subsequent per-workspace edits should target. */
export function forkWorkspace(
  prefs: Preferences,
  agent: string,
  workspace: string,
): Preferences {
  const key = workspaceKey(agent, workspace);
  if (key in prefs.workspaces) return prefs;
  const snapshot = cloneScoped({ env: prefs.env, toolArgs: prefs.toolArgs });
  return {
    ...prefs,
    workspaces: { ...prefs.workspaces, [key]: snapshot },
  };
}

/** Drop a workspace's fork so it re-inherits the global config live. */
export function resetWorkspaceConfig(
  prefs: Preferences,
  agent: string,
  workspace: string,
): Preferences {
  const key = workspaceKey(agent, workspace);
  if (!(key in prefs.workspaces)) return prefs;
  const next = { ...prefs.workspaces };
  delete next[key];
  return { ...prefs, workspaces: next };
}

// ── Workspace sort / pin helpers ─────────────────────────────────────────────
//
// Ordering model (shared verbatim with the cloudcode CLI menu):
//   - Pinned workspaces float to the top as one group; the rest follow.
//   - Within each group, items the user has explicitly ordered come first
//     (by their stored `rank`), then any untouched ones by the default
//     online-first / agent↑ / name↑ order.
//   - Pin/move always re-materialise ranks for the whole visible list, so
//     subsequent moves are deterministic; entries for workspaces not in the
//     current list are preserved (an offline agent's pins survive).

/** The default order when a workspace has no stored rank. */
function defaultCompare(a: Orderable, b: Orderable): number {
  const online = (b.agent_online ? 1 : 0) - (a.agent_online ? 1 : 0);
  if (online !== 0) return online;
  if (a.agent !== b.agent) return a.agent.localeCompare(b.agent);
  return a.name.localeCompare(b.name);
}

/** Whether a workspace is pinned. */
export function isPinned(prefs: Preferences, agent: string, name: string): boolean {
  return prefs.workspaceOrder[workspaceKey(agent, name)]?.pinned === true;
}

/** Sort a workspace list by the user's pin/rank preferences. Pure. */
export function sortByPreference<T extends Orderable>(prefs: Preferences, items: T[]): T[] {
  const order = prefs.workspaceOrder;
  return [...items].sort((a, b) => {
    const ea = order[workspaceKey(a.agent, a.name)];
    const eb = order[workspaceKey(b.agent, b.name)];
    const pa = ea?.pinned ? 1 : 0;
    const pb = eb?.pinned ? 1 : 0;
    if (pa !== pb) return pb - pa; // pinned group first
    if (ea && eb) {
      if (ea.rank !== eb.rank) return ea.rank - eb.rank;
      return defaultCompare(a, b);
    }
    if (ea) return -1; // user-ordered before untouched (same pinned-ness)
    if (eb) return 1;
    return defaultCompare(a, b);
  });
}

/** Re-materialise ranks for `finalKeys` (the new display order), preserving
 *  entries for any workspace not currently visible. */
function applyOrder(
  prefs: Preferences,
  finalKeys: string[],
  pinnedSet: Set<string>,
): Preferences {
  const next: Record<string, WorkspaceOrderEntry> = { ...prefs.workspaceOrder };
  finalKeys.forEach((k, i) => {
    next[k] = { pinned: pinnedSet.has(k), rank: i };
  });
  return { ...prefs, workspaceOrder: next };
}

/** Toggle a workspace's pinned state, placing it at its new group's edge. */
export function togglePin<T extends Orderable>(
  prefs: Preferences,
  items: T[],
  agent: string,
  name: string,
): Preferences {
  const key = workspaceKey(agent, name);
  const willPin = !isPinned(prefs, agent, name);
  const keys = sortByPreference(prefs, items).map((w) => workspaceKey(w.agent, w.name));
  const pinnedSet = new Set(keys.filter((k) => prefs.workspaceOrder[k]?.pinned));
  if (willPin) pinnedSet.add(key);
  else pinnedSet.delete(key);
  const without = keys.filter((k) => k !== key);
  const pinnedPart = without.filter((k) => pinnedSet.has(k));
  const normalPart = without.filter((k) => !pinnedSet.has(k));
  if (willPin) pinnedPart.push(key); // newly pinned → bottom of pinned group
  else normalPart.unshift(key); // newly unpinned → top of the rest
  return applyOrder(prefs, [...pinnedPart, ...normalPart], pinnedSet);
}

/** Move a workspace one slot up/down within its own group (pinned or not). */
export function moveWorkspace<T extends Orderable>(
  prefs: Preferences,
  items: T[],
  agent: string,
  name: string,
  dir: 'up' | 'down',
): Preferences {
  const key = workspaceKey(agent, name);
  const keys = sortByPreference(prefs, items).map((w) => workspaceKey(w.agent, w.name));
  const pinnedSet = new Set(keys.filter((k) => prefs.workspaceOrder[k]?.pinned));
  const inPinned = pinnedSet.has(key);
  const group = keys.filter((k) => pinnedSet.has(k) === inPinned);
  const idx = group.indexOf(key);
  if (idx < 0) return prefs;
  const swap = dir === 'up' ? idx - 1 : idx + 1;
  if (swap < 0 || swap >= group.length) return prefs; // at the edge — no-op
  [group[idx], group[swap]] = [group[swap], group[idx]];
  const pinnedPart = inPinned ? group : keys.filter((k) => pinnedSet.has(k));
  const normalPart = inPinned ? keys.filter((k) => !pinnedSet.has(k)) : group;
  return applyOrder(prefs, [...pinnedPart, ...normalPart], pinnedSet);
}

// ── Args ↔ text helpers ─────────────────────────────────────────────────────

/** Display args as a single string for an <input>. Naive: just space-join.
 *  Round-trips correctly as long as users don't put spaces inside args. */
export function argsToText(args: string[]): string {
  return args.join(' ');
}

/** Parse the text typed into the input back into argv. Whitespace-split,
 *  empty entries dropped. Doesn't handle quoted args — keep it dumb until
 *  someone hits a real need (claude/codex flags rarely contain spaces). */
export function textToArgs(text: string): string[] {
  return text
    .trim()
    .split(/\s+/)
    .filter((s) => s.length > 0);
}
