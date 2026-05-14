import { useEffect, useState, type FormEvent, type ReactNode } from 'react';
import { useSearchParams } from 'react-router-dom';
import { apiClient, type AuditEventDto } from '@/lib/api';
import { formatDateTime } from '@/lib/time';
import { Modal } from '@/components/Modal';

export function Audit() {
  const [params, setParams] = useSearchParams();
  const [events, setEvents] = useState<AuditEventDto[] | null>(null);
  const [total, setTotal] = useState(0);
  const [pageSize, setPageSize] = useState(50);
  const [kinds, setKinds] = useState<string[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [detail, setDetail] = useState<AuditEventDto | null>(null);

  // Form state is detached from URL so typing doesn't refetch on every keystroke.
  const [form, setForm] = useState({
    account: params.get('account') ?? '',
    agent: params.get('agent') ?? '',
    kind: params.get('kind') ?? '',
    since: params.get('since') ?? '',
    until: params.get('until') ?? '',
  });

  const page = parseInt(params.get('page') ?? '1', 10) || 1;

  useEffect(() => {
    apiClient.audit
      .kinds()
      .then(setKinds)
      .catch(() => {});
  }, []);

  useEffect(() => {
    let cancelled = false;
    setEvents(null);
    setErr(null);
    apiClient.audit
      .list({
        account: params.get('account') ?? undefined,
        agent: params.get('agent') ?? undefined,
        kind: params.get('kind') ?? undefined,
        since: params.get('since') ?? undefined,
        until: params.get('until') ?? undefined,
        page,
        limit: 50,
      })
      .then((r) => {
        if (cancelled) return;
        setEvents(r.events);
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
    if (form.kind) next.set('kind', form.kind);
    if (form.since) next.set('since', form.since);
    if (form.until) next.set('until', form.until);
    setParams(next);
  }

  function resetFilters() {
    setForm({ account: '', agent: '', kind: '', since: '', until: '' });
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
      <h2 className="text-base font-semibold">Audit timeline</h2>

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
        <Field label="Kind">
          <select
            value={form.kind}
            onChange={(e) => setForm((s) => ({ ...s, kind: e.target.value }))}
            className={inputCls}
          >
            <option value="">(any)</option>
            {kinds.map((k) => (
              <option key={k} value={k}>
                {k}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Since (UTC)">
          <input
            type="datetime-local"
            value={form.since}
            onChange={(e) => setForm((s) => ({ ...s, since: e.target.value }))}
            className={inputCls}
          />
        </Field>
        <Field label="Until (UTC)">
          <input
            type="datetime-local"
            value={form.until}
            onChange={(e) => setForm((s) => ({ ...s, until: e.target.value }))}
            className={inputCls}
          />
        </Field>
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
        {total} event{total === 1 ? '' : 's'} match
      </div>

      <div className="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
        <table className="w-full text-sm">
          <thead className="bg-zinc-50 dark:bg-zinc-900/50 text-xs uppercase tracking-wide text-zinc-500">
            <tr>
              <th className="px-3 py-2 text-left">Time (UTC)</th>
              <th className="px-3 py-2 text-left">Kind</th>
              <th className="px-3 py-2 text-left">Account</th>
              <th className="px-3 py-2 text-left">Agent</th>
              <th className="px-3 py-2 text-left">Workspace</th>
              <th className="px-3 py-2 text-left">Session</th>
              <th className="px-3 py-2 text-left">Detail</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-zinc-200 dark:divide-zinc-800 bg-white dark:bg-zinc-900">
            {events === null ? (
              <tr>
                <td colSpan={7} className="px-3 py-6 text-center text-zinc-500">
                  Loading…
                </td>
              </tr>
            ) : events.length === 0 ? (
              <tr>
                <td colSpan={7} className="px-3 py-6 text-center text-zinc-500">
                  No events match these filters.
                </td>
              </tr>
            ) : (
              events.map((ev) => (
                <tr key={ev.id} className="hover:bg-zinc-50 dark:hover:bg-zinc-800/30">
                  <td className="px-3 py-2 whitespace-nowrap font-mono text-xs">{formatTs(ev.ts)}</td>
                  <td className="px-3 py-2 font-mono text-xs">{ev.kind}</td>
                  <td className="px-3 py-2 font-mono text-xs">{ev.account ?? <Dim>—</Dim>}</td>
                  <td className="px-3 py-2 font-mono text-xs">{ev.agent ?? <Dim>—</Dim>}</td>
                  <td className="px-3 py-2 font-mono text-xs">{ev.workspace ?? <Dim>—</Dim>}</td>
                  <td className="px-3 py-2 font-mono text-xs text-zinc-500">
                    {ev.session_id ? ev.session_id.slice(0, 8) : ''}
                  </td>
                  <td className="px-3 py-2 max-w-md">
                    {ev.detail ? (
                      <button
                        onClick={() => setDetail(ev)}
                        className="text-xs text-zinc-500 underline-offset-2 hover:underline hover:text-zinc-900 dark:hover:text-zinc-100 text-left truncate block w-full"
                        title="Show full JSON"
                      >
                        {summary(ev.detail)}
                      </button>
                    ) : (
                      <Dim>—</Dim>
                    )}
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

      <Modal
        open={detail !== null}
        onClose={() => setDetail(null)}
        title="Event detail"
        footer={
          <button
            onClick={() => setDetail(null)}
            className="px-3 py-1.5 text-sm rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90"
          >
            Close
          </button>
        }
      >
        {detail && (
          <>
            <dl className="text-xs grid grid-cols-[6rem_1fr] gap-y-1 gap-x-2">
              <Meta k="kind" v={detail.kind} />
              <Meta k="time" v={formatTs(detail.ts)} />
              {detail.account && <Meta k="account" v={detail.account} />}
              {detail.agent && <Meta k="agent" v={detail.agent} />}
              {detail.workspace && <Meta k="workspace" v={detail.workspace} />}
              {detail.session_id && <Meta k="session" v={detail.session_id} />}
            </dl>
            <pre className="rounded bg-zinc-950 text-zinc-100 p-3 text-xs overflow-x-auto whitespace-pre-wrap break-words">
              {JSON.stringify(detail.detail ?? {}, null, 2)}
            </pre>
          </>
        )}
      </Modal>
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

function Meta({ k, v }: { k: string; v: string }) {
  return (
    <>
      <dt className="text-zinc-500">{k}</dt>
      <dd>
        <code className="text-zinc-900 dark:text-zinc-100">{v}</code>
      </dd>
    </>
  );
}

function formatTs(unix: number): string {
  return formatDateTime(unix);
}

function summary(detail: Record<string, unknown>): string {
  const parts: string[] = [];
  if (typeof detail.status === 'number') parts.push(`status=${detail.status}`);
  if (typeof detail.exit_code === 'number') parts.push(`exit=${detail.exit_code}`);
  if (typeof detail.reason === 'string') parts.push(detail.reason);
  if (parts.length === 0) return JSON.stringify(detail).slice(0, 100);
  return parts.join(' · ');
}
