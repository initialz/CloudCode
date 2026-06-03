// Shared form primitives for editing a ScopedConfig ({ env, toolArgs }).
// Used by both the global SettingsDialog and the per-workspace
// ConfigDialog so the two stay in sync.

import { KNOWN_TOOLS, type Tool } from '@/lib/tools';
import {
  argsToText,
  textToArgs,
  ENV_KEY_RE,
  type EnvMap,
  type ScopedConfig,
} from '@/lib/preferences';

// Tools whose args we expose in the UI. KNOWN_TOOLS stays the source of
// truth for "can be launched"; this is a narrower set — codex args are
// hidden until we have a clear UX for codex-specific flags.
export const CONFIGURABLE_TOOLS: Tool[] = KNOWN_TOOLS.filter((t) => t !== 'codex');

// A single env row in the editor. We keep rows as an ordered array (with
// a stable id) rather than an object so empty/duplicate keys are editable
// mid-typing without rows jumping around.
export type EnvRow = { id: string; key: string; value: string };

let rowSeq = 0;
function newRowId(): string {
  rowSeq += 1;
  return `env-${rowSeq}-${Math.random().toString(36).slice(2, 8)}`;
}

/** Build editor rows from a stored env map. */
export function envToRows(env: EnvMap): EnvRow[] {
  return Object.entries(env).map(([key, value]) => ({
    id: newRowId(),
    key,
    value,
  }));
}

/** Collapse editor rows back into a stored env map. Empty-key rows are
 *  dropped; later rows win on duplicate keys. */
export function rowsToEnv(rows: EnvRow[]): EnvMap {
  const out: EnvMap = {};
  for (const r of rows) {
    const key = r.key.trim();
    if (!key) continue;
    out[key] = r.value;
  }
  return out;
}

/** A row whose key is non-empty and fails the identifier rule. */
export function invalidRowIds(rows: EnvRow[]): Set<string> {
  const bad = new Set<string>();
  for (const r of rows) {
    const key = r.key.trim();
    if (key && !ENV_KEY_RE.test(key)) bad.add(r.id);
  }
  return bad;
}

export function emptyRow(): EnvRow {
  return { id: newRowId(), key: '', value: '' };
}

// ── Args editor ──────────────────────────────────────────────────────────────

export function ArgsEditor({
  argsText,
  onChange,
}: {
  argsText: Record<Tool, string>;
  onChange: (tool: Tool, text: string) => void;
}) {
  return (
    <div>
      <p className="text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-2 uppercase tracking-wide">
        Launch args per tool
      </p>
      <p className="text-xs text-zinc-500 dark:text-zinc-400 mb-3 leading-snug">
        Appended whenever you launch the tool. Whitespace-separated; quoted
        args aren&apos;t supported.
      </p>
      <div className="space-y-2">
        {CONFIGURABLE_TOOLS.map((tool) => (
          <label key={tool} className="flex items-center gap-3">
            <span className="w-16 shrink-0 text-xs font-mono text-zinc-700 dark:text-zinc-300 capitalize">
              {tool}
            </span>
            <input
              type="text"
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              placeholder="--flag value …"
              value={argsText[tool] ?? ''}
              onChange={(e) => onChange(tool, e.target.value)}
              className="flex-1 px-2 py-1.5 text-xs font-mono rounded-md border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-950 text-zinc-900 dark:text-zinc-100 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:focus:ring-zinc-500"
            />
          </label>
        ))}
      </div>
    </div>
  );
}

// ── Env editor ───────────────────────────────────────────────────────────────

export function EnvEditor({
  rows,
  onChange,
}: {
  rows: EnvRow[];
  onChange: (rows: EnvRow[]) => void;
}) {
  const invalid = invalidRowIds(rows);

  function updateRow(id: string, patch: Partial<EnvRow>) {
    onChange(rows.map((r) => (r.id === id ? { ...r, ...patch } : r)));
  }
  function removeRow(id: string) {
    onChange(rows.filter((r) => r.id !== id));
  }
  function addRow() {
    onChange([...rows, emptyRow()]);
  }

  return (
    <div>
      <p className="text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-2 uppercase tracking-wide">
        Environment variables
      </p>
      <p className="text-xs text-zinc-500 dark:text-zinc-400 mb-3 leading-snug">
        Set when launching any tool. Values are stored as entered (plaintext).
      </p>
      <div className="space-y-2">
        {rows.length === 0 && (
          <p className="text-xs text-zinc-400 dark:text-zinc-500 italic">
            No variables set.
          </p>
        )}
        {rows.map((row) => {
          const isInvalid = invalid.has(row.id);
          return (
            <div key={row.id}>
              <div className="flex items-center gap-2">
                <input
                  type="text"
                  spellCheck={false}
                  autoCapitalize="off"
                  autoCorrect="off"
                  placeholder="KEY"
                  value={row.key}
                  onChange={(e) => updateRow(row.id, { key: e.target.value })}
                  className={`w-1/3 shrink-0 px-2 py-1.5 text-xs font-mono rounded-md border bg-white dark:bg-zinc-950 text-zinc-900 dark:text-zinc-100 focus:outline-none focus:ring-1 ${
                    isInvalid
                      ? 'border-red-400 dark:border-red-500 focus:ring-red-400'
                      : 'border-zinc-200 dark:border-zinc-700 focus:ring-zinc-400 dark:focus:ring-zinc-500'
                  }`}
                />
                <input
                  type="text"
                  spellCheck={false}
                  autoCapitalize="off"
                  autoCorrect="off"
                  placeholder="value"
                  value={row.value}
                  onChange={(e) => updateRow(row.id, { value: e.target.value })}
                  className="flex-1 px-2 py-1.5 text-xs font-mono rounded-md border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-950 text-zinc-900 dark:text-zinc-100 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:focus:ring-zinc-500"
                />
                <button
                  type="button"
                  onClick={() => removeRow(row.id)}
                  aria-label="Remove variable"
                  className="shrink-0 rounded-md p-1.5 text-zinc-400 hover:text-red-600 dark:hover:text-red-400 hover:bg-zinc-100 dark:hover:bg-zinc-800 transition-colors"
                >
                  <MinusIcon />
                </button>
              </div>
              {isInvalid && (
                <p className="mt-1 text-[10px] text-red-600 dark:text-red-400">
                  Invalid key — use letters, digits, underscore; must not start
                  with a digit.
                </p>
              )}
            </div>
          );
        })}
      </div>
      <button
        type="button"
        onClick={addRow}
        className="mt-2 flex items-center gap-1.5 text-xs text-zinc-500 dark:text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100 transition-colors"
      >
        <PlusIcon />
        Add variable
      </button>
    </div>
  );
}

// ── ScopedConfig form helpers ────────────────────────────────────────────────

/** Seed the per-tool args text inputs from a ScopedConfig. */
export function initialArgsText(cfg: ScopedConfig): Record<Tool, string> {
  return Object.fromEntries(
    CONFIGURABLE_TOOLS.map((t) => [t, argsToText(cfg.toolArgs[t] ?? [])]),
  ) as Record<Tool, string>;
}

/** Collapse env rows + args text into a full ScopedConfig. Args for
 *  non-configurable tools are carried over from `base` unchanged. */
export function collapseConfig(
  base: ScopedConfig,
  envRows: EnvRow[],
  argsText: Record<Tool, string>,
): ScopedConfig {
  const toolArgs = { ...base.toolArgs };
  for (const tool of CONFIGURABLE_TOOLS) {
    toolArgs[tool] = textToArgs(argsText[tool] ?? '');
  }
  return {
    env: rowsToEnv(envRows),
    toolArgs,
  };
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

function MinusIcon() {
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
      <path d="M5 12h14" />
    </svg>
  );
}
