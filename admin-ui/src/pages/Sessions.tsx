import { useEffect, useState, type FormEvent, type ReactNode } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { apiClient, type SessionDto } from '@/lib/api';
import { formatDateTime } from '@/lib/time';

export function Sessions() {
  const [params, setParams] = useSearchParams();
  const [sessions, setSessions] = useState<SessionDto[] | null>(null);
  const [total, setTotal] = useState(0);
  const [pageSize, setPageSize] = useState(50);
  const [err, setErr] = useState<string | null>(null);

  const [form, setForm] = useState({
    account: params.get('account') ?? '',
    agent: params.get('agent') ?? '',
    workspace: params.get('workspace') ?? '',
    active: params.get('active') === '1',
  });

  const page = parseInt(params.get('page') ?? '1', 10) || 1;

  useEffect(() => {
    let cancelled = false;
    setSessions(null);
    setErr(null);
    apiClient.sessions
      .list({
        account: params.get('account') ?? undefined,
        agent: params.get('agent') ?? undefined,
        workspace: params.get('workspace') ?? undefined,
        active: params.get('active') === '1' ? true : undefined,
        page,
        limit: 50,
      })
      .then((r) => {
        if (cancelled) return;
        setSessions(r.sessions);
        setTotal(r.total);
        setPageSize(r.page_size);
      })
      .catch((e: any) => {
        if (cancelled) return;
        setErr(e?.message ?? 'load failed');
      });
    return () => {
      cancelled = true;
    };
  }, [params, page]);

  function applyFilters(e: FormEvent) {
    e.preventDefault();
    const next = new URLSearchParams();
    if (form.account) next.set('account', form.account);
    if (form.agent) next.set('agent', form.agent);
    if (form.workspace) next.set('workspace', form.workspace);
    if (form.active) next.set('active', '1');
    setParams(next);
  }

  function resetFilters() {
    setForm({ account: '', agent: '', workspace: '', active: false });
    setParams(new URLSearchParams());
  }

  function gotoPage(p: number) {
    const next = new URLSearchParams(params);
    if (p === 1) next.delete('page');
    else next.set('page', String(p));
    setParams(next);
  }

  const lastPage = Math.max(1, Math.ceil(total / pageSize));

  return (
    <div className="space-y-4">
      <h2 className="text-base font-semibold">Sessions</h2>

      <form
        onSubmit={applyFilters}
        className="rounded-lg border border-zinc-200 dark:border-zinc-800 p-3 bg-white dark:bg-zinc-900 flex flex-wrap gap-3 items-end"
      >
        <Field label="Account">
          <input
            value={form.account}
            onChange={(e) => setForm((s) => ({ ...s, account: e.target.value }))}
            placeholder="alice"
            className={inputCls}
          />
        </Field>
        <Field label="Agent">
          <input
            value={form.agent}
            onChange={(e) => setForm((s) => ({ ...s, agent: e.target.value }))}
            placeholder="petez-mbp"
            className={inputCls}
          />
        </Field>
        <Field label="Workspace">
          <input
            value={form.workspace}
            onChange={(e) => setForm((s) => ({ ...s, workspace: e.target.value }))}
            placeholder="proja"
            className={inputCls}
          />
        </Field>
        <label className="flex items-center gap-2 text-xs text-zinc-700 dark:text-zinc-300 pb-1.5">
          <input
            type="checkbox"
            checked={form.active}
            onChange={(e) => setForm((s) => ({ ...s, active: e.target.checked }))}
          />
          active only
        </label>
        <button
          type="submit"
          className="px-3 py-1.5 rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 text-sm hover:opacity-90"
        >
          Filter
        </button>
        <button
          type="button"
          onClick={resetFilters}
          className="px-3 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800"
        >
          Reset
        </button>
      </form>

      {err && (
        <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
          {err}
        </div>
      )}

      <div className="text-xs text-zinc-500">
        {total} session{total === 1 ? '' : 's'} match
      </div>

      <div className="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
        <table className="w-full text-sm">
          <thead className="bg-zinc-50 dark:bg-zinc-900/50 text-xs uppercase tracking-wide text-zinc-500">
            <tr>
              <th className="px-3 py-2 text-left">Started (UTC)</th>
              <th className="px-3 py-2 text-left">Duration</th>
              <th className="px-3 py-2 text-left">Account</th>
              <th className="px-3 py-2 text-left">Agent</th>
              <th className="px-3 py-2 text-left">Workspace</th>
              <th className="px-3 py-2 text-left">Session</th>
              <th className="px-3 py-2 text-left">Closed reason</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-zinc-200 dark:divide-zinc-800 bg-white dark:bg-zinc-900">
            {sessions === null ? (
              <tr>
                <td colSpan={7} className="px-3 py-6 text-center text-zinc-500">
                  Loading…
                </td>
              </tr>
            ) : sessions.length === 0 ? (
              <tr>
                <td colSpan={7} className="px-3 py-6 text-center text-zinc-500">
                  No sessions match these filters.
                </td>
              </tr>
            ) : (
              sessions.map((s) => (
                <tr key={s.session_id} className="hover:bg-zinc-50 dark:hover:bg-zinc-800/30">
                  <td className="px-3 py-2 whitespace-nowrap font-mono text-xs">
                    {formatTs(s.started_at)}
                  </td>
                  <td className="px-3 py-2 whitespace-nowrap">
                    {s.ended_at === null ? (
                      <span className="inline-flex items-center gap-1 text-xs text-green-600 dark:text-green-400">
                        <span className="w-2 h-2 rounded-full bg-green-500 animate-pulse" />
                        live
                      </span>
                    ) : (
                      <span className="text-xs text-zinc-500 font-mono">
                        {formatDuration(s.ended_at - s.started_at)}
                      </span>
                    )}
                  </td>
                  <td className="px-3 py-2 font-mono text-xs">{s.account}</td>
                  <td className="px-3 py-2 font-mono text-xs">{s.agent}</td>
                  <td className="px-3 py-2 font-mono text-xs">{s.workspace}</td>
                  <td className="px-3 py-2 font-mono text-xs">
                    <Link
                      to={`/sessions/${s.session_id}`}
                      className="text-zinc-700 dark:text-zinc-300 hover:underline"
                    >
                      {s.session_id.slice(0, 8)}
                    </Link>
                  </td>
                  <td className="px-3 py-2 text-xs text-zinc-500">
                    {s.ended_reason ?? <Dim>—</Dim>}
                  </td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>

      <div className="flex items-center justify-between">
        <button
          disabled={page <= 1}
          onClick={() => gotoPage(page - 1)}
          className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-30"
        >
          ← Prev
        </button>
        <span className="text-xs text-zinc-500">
          Page {page} of {lastPage}
        </span>
        <button
          disabled={page >= lastPage}
          onClick={() => gotoPage(page + 1)}
          className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-30"
        >
          Next →
        </button>
      </div>
    </div>
  );
}

const inputCls =
  'px-2 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent text-sm focus:outline-none focus:ring-2 focus:ring-zinc-400';

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="flex flex-col gap-1 text-xs text-zinc-500">
      {label}
      {children}
    </label>
  );
}

function Dim({ children }: { children: ReactNode }) {
  return <span className="text-zinc-400">{children}</span>;
}

function formatTs(unix: number): string {
  return formatDateTime(unix);
}

function formatDuration(seconds: number): string {
  if (seconds < 0) return '—';
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}
