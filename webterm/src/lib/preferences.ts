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

export type Preferences = ScopedConfig & {
  /** Per-workspace forked snapshots, keyed by `<agent>/<workspace>`.
   *  A key present means the workspace has been customised (forked);
   *  a key absent means it inherits the global config live. */
  workspaces: Record<string, ScopedConfig>;
};

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

type WireShape = WireScoped & {
  workspaces?: Record<string, WireScoped>;
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
    };
  }
  const wire = blob as WireShape;
  const base: Preferences = {
    ...parseScoped(wire),
    workspaces: {},
  };
  if (wire.workspaces && typeof wire.workspaces === 'object') {
    for (const [key, val] of Object.entries(wire.workspaces)) {
      if (typeof key !== 'string' || !key) continue;
      if (!val || typeof val !== 'object') continue;
      base.workspaces[key] = parseScoped(val as WireScoped);
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
