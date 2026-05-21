import { useEffect, useMemo, useRef, useState } from 'react';
import { apiClient, type AccountDto, type ActivityDto, type AgentRowDto } from '@/lib/api';
import { formatDateTime } from '@/lib/time';

const LIMIT = 50;
const ALL = '__all__';

// Epoch ms from a datetime-local string ("2024-01-15T09:00") or empty -> undefined
function datetimeToMs(val: string): number | undefined {
  if (!val) return undefined;
  const ms = new Date(val).getTime();
  return isNaN(ms) ? undefined : ms;
}

// Extract a short summary string from an audit detail object
function auditSummary(detail: Record<string, unknown>): string {
  const parts: string[] = [];
  if (typeof detail.status === 'number') parts.push(`status=${detail.status}`);
  if (typeof detail.exit_code === 'number') parts.push(`exit=${detail.exit_code}`);
  if (typeof detail.reason === 'string') parts.push(detail.reason);
  if (typeof detail.error === 'string') parts.push(`error=${detail.error}`);
  if (parts.length === 0) return JSON.stringify(detail).slice(0, 120);
  return parts.join(' · ');
}

// Unique key per row for expand state tracking
function rowKey(item: ActivityDto): string {
  return `${item.source}-${item.id}`;
}

export function Activity() {
  // ── data ──────────────────────────────────────────────────────────────────
  const [data, setData] = useState<{ items: ActivityDto[]; total: number } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  // ── expand state (reset on page/refresh) ──────────────────────────────────
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  // ── filter form state (uncommitted until Apply) ───────────────────────────
  const [sourceFilter, setSourceFilter] = useState<'all' | 'audit' | 'interaction'>('all');
  const [accountFilter, setAccountFilter] = useState<string>(ALL);
  const [workspaceInput, setWorkspaceInput] = useState('');
  const [agentFilter, setAgentFilter] = useState<string>(ALL);
  // Kind is multi-select: `kindChips` are the committed selections,
  // shown as removable chips. The picker below adds chips one by
  // one (re-picking is allowed but no-ops; an already-chipped option
  // is hidden from the dropdown so re-pick doesn't dupe).
  const [kindChips, setKindChips] = useState<string[]>([]);
  const [sinceInput, setSinceInput] = useState('');
  const [untilInput, setUntilInput] = useState('');

  // ── committed filter (triggers fetch when changed) ────────────────────────
  const [committed, setCommitted] = useState({
    source: 'all' as 'all' | 'audit' | 'interaction',
    account: ALL,
    workspace: '',
    agent: ALL,
    kinds: [] as string[],
    since: '',
    until: '',
  });

  // ── pagination ────────────────────────────────────────────────────────────
  const [offset, setOffset] = useState(0);

  // ── account / agent / kind lookups for the filter dropdowns ──────────────
  const [accounts, setAccounts] = useState<AccountDto[]>([]);
  const [agents, setAgents] = useState<AgentRowDto[]>([]);
  const [availableKinds, setAvailableKinds] = useState<string[]>([]);

  useEffect(() => {
    apiClient.accounts.list().then(setAccounts).catch(() => {});
    apiClient.agents.list().then(setAgents).catch(() => {});
    apiClient.activity.kinds().then(setAvailableKinds).catch(() => {});
  }, []);

  // ── fetch ─────────────────────────────────────────────────────────────────
  async function fetchData(newOffset = offset) {
    setLoading(true);
    setErr(null);
    try {
      const result = await apiClient.activity.list({
        source: committed.source !== 'all' ? committed.source : undefined,
        account: committed.account !== ALL ? committed.account : undefined,
        workspace: committed.workspace || undefined,
        agent: committed.agent !== ALL ? committed.agent : undefined,
        // Comma-separated; backend splits and emits `kind IN (...)`.
        kind: committed.kinds.length > 0 ? committed.kinds.join(',') : undefined,
        since_ms: datetimeToMs(committed.since),
        until_ms: datetimeToMs(committed.until),
        limit: LIMIT,
        offset: newOffset,
      });
      setData(result);
      setExpanded(new Set()); // reset expand on every fetch
    } catch (e: unknown) {
      const msg = typeof e === 'object' && e && 'message' in e
        ? String((e as { message?: unknown }).message)
        : 'failed to load activity';
      setErr(msg);
    } finally {
      setLoading(false);
    }
  }

  // initial load
  useEffect(() => {
    fetchData(0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [committed]);

  function handleApply() {
    const next = {
      source: sourceFilter,
      account: accountFilter,
      workspace: workspaceInput,
      agent: agentFilter,
      kinds: kindChips,
      since: sinceInput,
      until: untilInput,
    };
    setOffset(0);
    setCommitted(next);
  }


  function handleRefresh() {
    fetchData(offset);
  }

  function handlePrev() {
    const next = Math.max(0, offset - LIMIT);
    setOffset(next);
    fetchData(next);
  }

  function handleNext() {
    if (!data) return;
    const next = offset + LIMIT;
    if (next >= data.total) return;
    setOffset(next);
    fetchData(next);
  }

  function toggleExpand(key: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  // ── derived ───────────────────────────────────────────────────────────────
  const items = useMemo(() => data?.items ?? [], [data]);
  const total = data?.total ?? 0;
  const pageStart = total > 0 ? offset + 1 : 0;
  const pageEnd = data ? Math.min(offset + LIMIT, total) : 0;
  const hasPrev = offset > 0;
  const hasNext = data ? offset + LIMIT < total : false;

  return (
    <div className="space-y-4">
      {/* ── Header ── */}
      <div className="flex items-center justify-between">
        <h2 className="text-base font-semibold">Activity</h2>
        <button
          onClick={handleRefresh}
          disabled={loading}
          className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-40"
        >
          Refresh
        </button>
      </div>

      {/* ── Error bar ── */}
      {err && (
        <div className="rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-sm text-red-700 dark:text-red-300">
          {err}
        </div>
      )}

      {/* ── Filter bar ── */}
      <div className="rounded-lg border border-zinc-200 dark:border-zinc-800 p-3 bg-white dark:bg-zinc-900 flex flex-wrap gap-3 items-end text-sm">
        {/* Account */}
        <label className="flex flex-col gap-1 text-xs text-zinc-500">
          Account
          <select
            value={accountFilter}
            onChange={(e) => setAccountFilter(e.target.value)}
            className={inputCls}
          >
            <option value={ALL}>all</option>
            {accounts.map((a) => (
              <option key={a.name} value={a.name}>
                {a.name}
              </option>
            ))}
          </select>
        </label>

        {/* Agent */}
        <label className="flex flex-col gap-1 text-xs text-zinc-500">
          Agent
          <select
            value={agentFilter}
            onChange={(e) => setAgentFilter(e.target.value)}
            className={inputCls}
          >
            <option value={ALL}>all</option>
            {agents.map((a) => (
              <option key={a.name} value={a.name}>
                {a.name}
              </option>
            ))}
          </select>
        </label>

        {/* Workspace */}
        <label className="flex flex-col gap-1 text-xs text-zinc-500">
          Workspace
          <input
            type="text"
            value={workspaceInput}
            onChange={(e) => setWorkspaceInput(e.target.value)}
            placeholder="any"
            className={inputCls + ' w-32'}
          />
        </label>

        {/* Source */}
        <label className="flex flex-col gap-1 text-xs text-zinc-500">
          Source
          <select
            value={sourceFilter}
            onChange={(e) => setSourceFilter(e.target.value as 'all' | 'audit' | 'interaction')}
            className={inputCls}
          >
            <option value="all">all</option>
            <option value="audit">audit</option>
            <option value="interaction">interaction</option>
          </select>
        </label>

        {/* Kind — popover multi-select with checkbox list */}
        <label className="flex flex-col gap-1 text-xs text-zinc-500">
          Kind
          <KindMultiSelect
            selected={kindChips}
            options={availableKinds}
            onChange={setKindChips}
          />
        </label>

        {/* Since */}
        <label className="flex flex-col gap-1 text-xs text-zinc-500">
          Since (local)
          <input
            type="datetime-local"
            value={sinceInput}
            onChange={(e) => setSinceInput(e.target.value)}
            className={inputCls}
          />
        </label>

        {/* Until */}
        <label className="flex flex-col gap-1 text-xs text-zinc-500">
          Until (local)
          <input
            type="datetime-local"
            value={untilInput}
            onChange={(e) => setUntilInput(e.target.value)}
            className={inputCls}
          />
        </label>

        {/* Search */}
        <button
          onClick={handleApply}
          className="px-3 py-1.5 rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 text-sm hover:opacity-90 self-end"
        >
          Search
        </button>

        {/* Count */}
        {data && (
          <div className="ml-auto text-xs text-zinc-500 self-end pb-1">
            {pageStart}–{pageEnd} / {total} total
          </div>
        )}
      </div>

      {/* ── Table ── */}
      {data === null && !err ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-50 dark:bg-zinc-900/50 text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2 text-left">Account</th>
                <th className="px-3 py-2 text-left">Agent</th>
                <th className="px-3 py-2 text-left">Workspace</th>
                <th className="px-3 py-2 text-left">Source</th>
                <th className="px-3 py-2 text-left">Kind</th>
                <th className="px-3 py-2 text-left">Detail</th>
                <th className="px-3 py-2 text-left whitespace-nowrap">Timestamp</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-200 dark:divide-zinc-800 bg-white dark:bg-zinc-900">
              {loading ? (
                <tr>
                  <td colSpan={7} className="px-3 py-6 text-center text-zinc-500">
                    Loading…
                  </td>
                </tr>
              ) : items.length === 0 ? (
                <tr>
                  <td colSpan={7} className="px-3 py-6 text-center text-zinc-500">
                    No activity matches the current filters.
                  </td>
                </tr>
              ) : (
                items.map((item) => {
                  const key = rowKey(item);
                  const isExpanded = expanded.has(key);
                  return (
                    <ActivityRow
                      key={key}
                      item={item}
                      isExpanded={isExpanded}
                      onToggle={() => toggleExpand(key)}
                    />
                  );
                })
              )}
            </tbody>
          </table>
        </div>
      )}

      {/* ── Pagination ── */}
      {data && total > LIMIT && (
        <div className="flex items-center justify-between text-sm">
          <button
            onClick={handlePrev}
            disabled={!hasPrev || loading}
            className="px-3 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            Prev
          </button>
          <span className="text-xs text-zinc-500">
            {pageStart}–{pageEnd} of {total}
          </span>
          <button
            onClick={handleNext}
            disabled={!hasNext || loading}
            className="px-3 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            Next
          </button>
        </div>
      )}
    </div>
  );
}

// ── Sub-components ────────────────────────────────────────────────────────────

interface ActivityRowProps {
  item: ActivityDto;
  isExpanded: boolean;
  onToggle: () => void;
}

function ActivityRow({ item, isExpanded, onToggle }: ActivityRowProps) {
  return (
    <tr className="hover:bg-zinc-50 dark:hover:bg-zinc-800/30 align-top">
      {/* Account */}
      <td className="px-3 py-2 font-mono text-xs">
        {item.account ?? <Dim>—</Dim>}
      </td>

      {/* Agent */}
      <td className="px-3 py-2 font-mono text-xs">
        {item.agent ?? <Dim>—</Dim>}
      </td>

      {/* Workspace */}
      <td className="px-3 py-2 font-mono text-xs">
        {item.workspace ?? <Dim>—</Dim>}
      </td>

      {/* Source badge */}
      <td className="px-3 py-2">
        <SourceBadge source={item.source} />
      </td>

      {/* Kind */}
      <td className="px-3 py-2 font-mono text-xs">{item.kind}</td>

      {/* Detail — the main expandable cell */}
      <td
        className="px-3 py-2 max-w-md cursor-pointer"
        onClick={onToggle}
        title={isExpanded ? 'Click to collapse' : 'Click to expand'}
      >
        <DetailCell item={item} isExpanded={isExpanded} />
      </td>

      {/* Timestamp */}
      <td className="px-3 py-2 whitespace-nowrap font-mono text-xs text-zinc-500">
        {formatDateTime(item.ts_ms / 1000)}
      </td>
    </tr>
  );
}

function DetailCell({ item, isExpanded }: { item: ActivityDto; isExpanded: boolean }) {
  if (!item.detail) {
    return <Dim>—</Dim>;
  }

  if (item.source === 'interaction') {
    const content = typeof item.detail.content === 'string' ? item.detail.content : '';
    const cwd = typeof item.detail.cwd === 'string' ? item.detail.cwd : null;
    const branch = typeof item.detail.git_branch === 'string' ? item.detail.git_branch : null;
    const promptId = typeof item.detail.prompt_id === 'string' ? item.detail.prompt_id : null;

    if (isExpanded) {
      return (
        <div className="space-y-1.5">
          <pre className="whitespace-pre-wrap break-words font-mono text-xs bg-zinc-100 dark:bg-zinc-800 rounded px-2 py-1.5 text-zinc-800 dark:text-zinc-200">
            {content || <Dim>(empty)</Dim>}
          </pre>
          {(cwd || branch || promptId) && (
            <p className="font-mono text-xs text-zinc-400 leading-relaxed">
              {cwd && <span>cwd: {cwd}</span>}
              {cwd && branch && <span className="mx-1">|</span>}
              {branch && <span>branch: {branch}</span>}
              {(cwd || branch) && promptId && <span className="mx-1">|</span>}
              {promptId && <span>prompt: {promptId}</span>}
            </p>
          )}
          <CollapseHint />
        </div>
      );
    }

    // collapsed: line-clamp-3
    return (
      <div className="space-y-0.5">
        <p className="font-mono text-xs text-zinc-700 dark:text-zinc-300 line-clamp-3 whitespace-pre-wrap break-words leading-relaxed">
          {content}
        </p>
        {content.length === 0 && <Dim>(empty)</Dim>}
        <ExpandHint />
      </div>
    );
  }

  // audit row
  const summary = auditSummary(item.detail);

  if (isExpanded) {
    return (
      <div className="space-y-1.5">
        <pre className="rounded bg-zinc-950 text-zinc-100 p-2 text-xs overflow-x-auto whitespace-pre-wrap break-words">
          {JSON.stringify(item.detail, null, 2)}
        </pre>
        <CollapseHint />
      </div>
    );
  }

  // collapsed: chips summary
  return (
    <div className="space-y-0.5">
      <p className="text-xs text-zinc-500 truncate" title={summary}>
        {summary}
      </p>
      <ExpandHint />
    </div>
  );
}

interface KindMultiSelectProps {
  selected: string[];
  options: string[];
  onChange: (next: string[]) => void;
}

// Native <select multiple> closes on first pick; we want sticky multi-
// selection plus a compact trigger that doesn't grow with the number of
// chips. This is the smallest hand-rolled popover that fits the bill —
// no deps, outside-click + Escape close, checkbox-style toggles.
function KindMultiSelect({ selected, options, onChange }: KindMultiSelectProps) {
  const MAX_CHIPS = 2;
  const [open, setOpen] = useState(false);
  const wrapperRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function handleClick(e: MouseEvent) {
      if (wrapperRef.current && !wrapperRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    function handleKey(e: KeyboardEvent) {
      if (e.key === 'Escape') setOpen(false);
    }
    document.addEventListener('mousedown', handleClick);
    document.addEventListener('keydown', handleKey);
    return () => {
      document.removeEventListener('mousedown', handleClick);
      document.removeEventListener('keydown', handleKey);
    };
  }, [open]);

  function toggle(k: string) {
    if (selected.includes(k)) {
      onChange(selected.filter((x) => x !== k));
    } else {
      onChange([...selected, k]);
    }
  }

  const visibleChips = selected.slice(0, MAX_CHIPS);
  const overflow = selected.length - visibleChips.length;

  return (
    <div ref={wrapperRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className={
          inputCls +
          ' flex items-center gap-1 min-w-[14rem] max-w-[20rem] cursor-pointer text-left'
        }
        aria-haspopup="listbox"
        aria-expanded={open}
      >
        {selected.length === 0 ? (
          <span className="text-zinc-400 text-xs">any</span>
        ) : (
          <>
            {visibleChips.map((k) => (
              <span
                key={k}
                className="inline-flex items-center gap-1 rounded bg-zinc-200 dark:bg-zinc-700 text-zinc-800 dark:text-zinc-200 px-1.5 py-0.5 text-xs font-mono whitespace-nowrap"
              >
                {k}
                <span
                  role="button"
                  tabIndex={0}
                  onClick={(e) => {
                    e.stopPropagation();
                    toggle(k);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      e.stopPropagation();
                      toggle(k);
                    }
                  }}
                  aria-label={`Remove ${k}`}
                  className="text-zinc-500 hover:text-red-600 dark:hover:text-red-400 leading-none cursor-pointer"
                >
                  ×
                </span>
              </span>
            ))}
            {overflow > 0 && (
              <span className="text-xs font-mono text-zinc-500 dark:text-zinc-400 whitespace-nowrap">
                +{overflow}
              </span>
            )}
          </>
        )}
        <span className="ml-auto text-zinc-400 text-xs leading-none">▾</span>
      </button>

      {open && (
        <div
          role="listbox"
          aria-multiselectable
          className="absolute z-20 mt-1 w-64 max-h-72 overflow-y-auto rounded-md border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-lg py-1"
        >
          {options.length === 0 ? (
            <div className="px-3 py-2 text-xs text-zinc-500">No kinds yet</div>
          ) : (
            options.map((k) => {
              const isSelected = selected.includes(k);
              return (
                <label
                  key={k}
                  className="flex items-center gap-2 px-3 py-1.5 text-xs font-mono hover:bg-zinc-100 dark:hover:bg-zinc-800 cursor-pointer"
                >
                  <input
                    type="checkbox"
                    checked={isSelected}
                    onChange={() => toggle(k)}
                    className="cursor-pointer"
                  />
                  <span className="text-zinc-900 dark:text-zinc-100 truncate">
                    {k}
                  </span>
                </label>
              );
            })
          )}
          {selected.length > 0 && (
            <>
              <div className="border-t border-zinc-200 dark:border-zinc-800 my-1" />
              <button
                type="button"
                onClick={() => onChange([])}
                className="w-full text-left px-3 py-1.5 text-xs text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800"
              >
                Clear ({selected.length})
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}

function SourceBadge({ source }: { source: 'audit' | 'interaction' }) {
  if (source === 'audit') {
    return (
      <span className="text-xs px-1.5 py-0.5 rounded bg-zinc-100 dark:bg-zinc-800 text-zinc-600 dark:text-zinc-400">
        audit
      </span>
    );
  }
  return (
    <span className="text-xs px-1.5 py-0.5 rounded bg-blue-100 dark:bg-blue-900/40 text-blue-700 dark:text-blue-300">
      interaction
    </span>
  );
}

function Dim({ children }: { children: React.ReactNode }) {
  return <span className="text-zinc-400">{children}</span>;
}

function ExpandHint() {
  return (
    <span className="text-xs text-zinc-400 select-none">
      expand
    </span>
  );
}

function CollapseHint() {
  return (
    <span className="text-xs text-zinc-400 select-none">
      collapse
    </span>
  );
}

const inputCls =
  'px-2 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent text-sm focus:outline-none focus:ring-2 focus:ring-zinc-400';
