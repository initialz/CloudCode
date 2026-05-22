import { useEffect, useMemo, useState } from 'react';
import { apiClient, type WorkspaceRowDto, type WorkspaceStatus } from '@/lib/api';
import { Modal } from '@/components/Modal';
import { formatRelative } from '@/lib/time';

const ALL = '__all__';

export function Workspaces() {
  const [rows, setRows] = useState<WorkspaceRowDto[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [agentFilter, setAgentFilter] = useState<string>(ALL);
  const [accountFilter, setAccountFilter] = useState<string>(ALL);
  const [deletingKey, setDeletingKey] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<WorkspaceRowDto | null>(
    null,
  );

  async function reload() {
    try {
      const list = await apiClient.workspaces.list();
      setRows(list);
    } catch (e: any) {
      setErr(e?.message ?? 'failed to load workspaces');
    }
  }

  async function performDelete(r: WorkspaceRowDto) {
    const label = `${r.workspace}@${r.agent} (${r.account})`;
    const key = `${r.agent}|${r.account}|${r.workspace}`;
    setDeletingKey(key);
    setErr(null);
    try {
      await apiClient.workspaces.delete({
        agent: r.agent,
        account: r.account,
        workspace: r.workspace,
      });
      setConfirmDelete(null);
      await reload();
    } catch (e: any) {
      setErr(e?.message ?? `failed to delete ${label}`);
    } finally {
      setDeletingKey(null);
    }
  }

  useEffect(() => {
    reload();
  }, []);

  const agents = useMemo(() => {
    if (!rows) return [];
    return Array.from(new Set(rows.map((r) => r.agent))).sort();
  }, [rows]);

  const accounts = useMemo(() => {
    if (!rows) return [];
    return Array.from(new Set(rows.map((r) => r.account))).sort();
  }, [rows]);

  const filtered = useMemo(() => {
    if (!rows) return [];
    return rows.filter((r) => {
      if (agentFilter !== ALL && r.agent !== agentFilter) return false;
      if (accountFilter !== ALL && r.account !== accountFilter) return false;
      return true;
    });
  }, [rows, agentFilter, accountFilter]);

  const totals = useMemo(() => {
    const t = { active: 0, saved: 0, fresh: 0 };
    for (const r of filtered) t[r.status] += 1;
    return t;
  }, [filtered]);

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-base font-semibold">Workspaces</h2>
        <button
          onClick={reload}
          className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
        >
          Refresh
        </button>
      </div>

      {err && (
        <div className="rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-sm text-red-700 dark:text-red-300">
          {err}
        </div>
      )}

      <div className="flex flex-wrap items-center gap-3 text-sm">
        <label className="flex items-center gap-2">
          <span className="text-zinc-500">Agent</span>
          <select
            value={agentFilter}
            onChange={(e) => setAgentFilter(e.target.value)}
            className="px-2 py-1 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent"
          >
            <option value={ALL}>all</option>
            {agents.map((a) => (
              <option key={a} value={a}>
                {a}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-2">
          <span className="text-zinc-500">Account</span>
          <select
            value={accountFilter}
            onChange={(e) => setAccountFilter(e.target.value)}
            className="px-2 py-1 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent"
          >
            <option value={ALL}>all</option>
            {accounts.map((a) => (
              <option key={a} value={a}>
                {a}
              </option>
            ))}
          </select>
        </label>
        <div className="ml-auto text-xs text-zinc-500">
          {filtered.length} total · <StatusDot status="active" />{' '}
          {totals.active} active · <StatusDot status="saved" /> {totals.saved}{' '}
          saved · {totals.fresh} fresh
        </div>
      </div>

      {rows === null ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-50 dark:bg-zinc-900/50 text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2 text-left">Workspace</th>
                <th className="px-3 py-2 text-left">Agent</th>
                <th className="px-3 py-2 text-left">Account</th>
                <th className="px-3 py-2 text-left">Status</th>
                <th className="px-3 py-2 text-left">Last started</th>
                <th className="px-3 py-2 text-right"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-200 dark:divide-zinc-800 bg-white dark:bg-zinc-900">
              {filtered.length === 0 ? (
                <tr>
                  <td
                    colSpan={6}
                    className="px-3 py-6 text-center text-zinc-500"
                  >
                    No workspaces match the current filter.
                  </td>
                </tr>
              ) : (
                filtered.map((r) => {
                  const rowKey = `${r.agent}|${r.account}|${r.workspace}`;
                  const isDeleting = deletingKey === rowKey;
                  return (
                    <tr key={rowKey}>
                      <td className="px-3 py-2 font-mono">{r.workspace}</td>
                      <td className="px-3 py-2 font-mono">
                        {r.agent}
                        {!r.agent_online && (
                          <span
                            className="ml-2 text-xs px-1.5 py-0.5 rounded bg-zinc-100 dark:bg-zinc-800 text-zinc-500"
                            title="agent is not currently connected"
                          >
                            offline
                          </span>
                        )}
                      </td>
                      <td className="px-3 py-2 font-mono">{r.account}</td>
                      <td className="px-3 py-2">
                        <StatusBadge status={r.status} />
                      </td>
                      <td className="px-3 py-2 text-zinc-500">
                        {r.last_started_at
                          ? formatRelative(r.last_started_at)
                          : '—'}
                      </td>
                      <td className="px-3 py-2 text-right">
                        <button
                          onClick={() => setConfirmDelete(r)}
                          disabled={isDeleting || r.status === 'active'}
                          title={
                            r.status === 'active'
                              ? 'cannot delete a workspace that has an active client'
                              : 'delete this workspace'
                          }
                          className="px-2 py-1 text-xs rounded border border-red-300 dark:border-red-800 text-red-700 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-950/30 disabled:opacity-40 disabled:cursor-not-allowed"
                        >
                          {isDeleting ? 'Deleting…' : 'Delete'}
                        </button>
                      </td>
                    </tr>
                  );
                })
              )}
            </tbody>
          </table>
        </div>
      )}

      <Modal
        open={confirmDelete !== null}
        onClose={() => deletingKey === null && setConfirmDelete(null)}
        title={
          confirmDelete
            ? `Delete workspace ${confirmDelete.workspace}@${confirmDelete.agent}?`
            : 'Delete workspace?'
        }
        footer={
          <>
            <button
              disabled={deletingKey !== null}
              onClick={() => setConfirmDelete(null)}
              className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              disabled={deletingKey !== null}
              onClick={() => confirmDelete && performDelete(confirmDelete)}
              className="px-3 py-1.5 text-sm rounded bg-red-600 text-white hover:bg-red-700 disabled:opacity-50"
            >
              {deletingKey !== null ? 'Deleting…' : 'Delete'}
            </button>
          </>
        }
      >
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          Removes the workspace dir from{' '}
          <span className="font-mono">{confirmDelete?.agent ?? ''}</span>'s
          disk and unbinds it from the hub for account{' '}
          <span className="font-mono">{confirmDelete?.account ?? ''}</span>.
          This cannot be undone.
        </p>
      </Modal>
    </div>
  );
}

// Match the cloudcode menu picker glyphs: red ● for an active client,
// yellow · for a saved-but-detached workspace, blank for fresh.
function StatusBadge({ status }: { status: WorkspaceStatus }) {
  if (status === 'active') {
    return (
      <span className="inline-flex items-center gap-1.5 text-xs">
        <span className="text-red-600 dark:text-red-400">●</span>
        <span>active</span>
      </span>
    );
  }
  if (status === 'saved') {
    return (
      <span className="inline-flex items-center gap-1.5 text-xs">
        <span className="text-yellow-600 dark:text-yellow-400">·</span>
        <span>saved</span>
      </span>
    );
  }
  return <span className="text-xs text-zinc-400">fresh</span>;
}

function StatusDot({ status }: { status: WorkspaceStatus }) {
  if (status === 'active') {
    return <span className="text-red-600 dark:text-red-400">●</span>;
  }
  if (status === 'saved') {
    return <span className="text-yellow-600 dark:text-yellow-400">·</span>;
  }
  return <span className="text-zinc-400">·</span>;
}

