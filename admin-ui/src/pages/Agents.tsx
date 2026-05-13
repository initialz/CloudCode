import { useEffect, useState } from 'react';
import { apiClient, type AgentRowDto, type AllowedAccountsDto } from '@/lib/api';
import { Modal } from '@/components/Modal';

type AccountsModalState = {
  agentName: string;
  data: AllowedAccountsDto | null;
  selected: Set<string>;
  loading: boolean;
  saving: boolean;
  err: string | null;
};

export function Agents() {
  const [agents, setAgents] = useState<AgentRowDto[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [accountsModal, setAccountsModal] = useState<AccountsModalState | null>(null);

  async function reload() {
    try {
      const list = await apiClient.agents.list();
      setAgents(list);
    } catch (e: any) {
      setErr(e?.message ?? 'failed to load agents');
    }
  }

  useEffect(() => {
    reload();
  }, []);

  async function openAccountsModal(agentName: string) {
    setAccountsModal({
      agentName,
      data: null,
      selected: new Set(),
      loading: true,
      saving: false,
      err: null,
    });
    try {
      const data = await apiClient.agents.allowedAccounts(agentName);
      setAccountsModal({
        agentName,
        data,
        selected: new Set(data.allowed),
        loading: false,
        saving: false,
        err: null,
      });
    } catch (e: any) {
      setAccountsModal((cur) =>
        cur && cur.agentName === agentName
          ? { ...cur, loading: false, err: e?.message ?? 'failed to load' }
          : cur,
      );
    }
  }

  function toggleAccount(name: string) {
    setAccountsModal((cur) => {
      if (!cur) return cur;
      const next = new Set(cur.selected);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return { ...cur, selected: next };
    });
  }

  async function saveAllowedAccounts() {
    if (!accountsModal) return;
    setAccountsModal({ ...accountsModal, saving: true, err: null });
    try {
      await apiClient.agents.setAllowedAccounts(
        accountsModal.agentName,
        Array.from(accountsModal.selected).sort(),
      );
      setAccountsModal(null);
      await reload();
    } catch (e: any) {
      setAccountsModal((cur) =>
        cur ? { ...cur, saving: false, err: e?.message ?? 'save failed' } : cur,
      );
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-base font-semibold">Agents</h2>
        <button
          onClick={reload}
          className="px-3 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800"
        >
          Refresh
        </button>
      </div>

      {err && (
        <div className="rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-sm text-red-700 dark:text-red-300">
          {err}
        </div>
      )}

      {agents === null ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-50 dark:bg-zinc-900/50 text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2 text-left">Name</th>
                <th className="px-3 py-2 text-left">Status</th>
                <th className="px-3 py-2 text-left">Accounts</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-200 dark:divide-zinc-800 bg-white dark:bg-zinc-900">
              {agents.length === 0 ? (
                <tr>
                  <td colSpan={3} className="px-3 py-6 text-center text-zinc-500">
                    No agents have ever connected to this hub yet.
                  </td>
                </tr>
              ) : (
                agents.map((a) => (
                  <tr key={a.name}>
                    <td className="px-3 py-2 font-mono">{a.name}</td>
                    <td className="px-3 py-2">
                      {a.online ? (
                        <span className="text-xs px-2 py-0.5 rounded bg-green-100 dark:bg-green-900/40 text-green-700 dark:text-green-300">
                          online
                        </span>
                      ) : (
                        <span className="text-xs px-2 py-0.5 rounded bg-zinc-100 dark:bg-zinc-800 text-zinc-800 dark:text-zinc-200">
                          offline
                        </span>
                      )}
                    </td>
                    <td className="px-3 py-2">
                      <button
                        onClick={() => openAccountsModal(a.name)}
                        className="text-xs font-mono px-2 py-0.5 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                        title="Edit account access"
                      >
                        {a.allowed_account_count === 0 ? (
                          <span className="text-red-600 dark:text-red-400">none</span>
                        ) : (
                          <>
                            {a.allowed_account_count} account
                            {a.allowed_account_count === 1 ? '' : 's'}
                          </>
                        )}
                      </button>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      )}

      <Modal
        open={accountsModal !== null}
        onClose={() => accountsModal && !accountsModal.saving && setAccountsModal(null)}
        title={
          accountsModal
            ? `Accounts allowed on ${accountsModal.agentName}`
            : 'Allowed accounts'
        }
        footer={
          <>
            <button
              disabled={accountsModal?.saving}
              onClick={() => setAccountsModal(null)}
              className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              disabled={!accountsModal || accountsModal.loading || accountsModal.saving}
              onClick={saveAllowedAccounts}
              className="px-3 py-1.5 text-sm rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90 disabled:opacity-50"
            >
              {accountsModal?.saving ? 'Saving…' : 'Save'}
            </button>
          </>
        }
      >
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          Strict whitelist — only the accounts checked below can connect through this agent.
        </p>
        {accountsModal?.err && (
          <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
            {accountsModal.err}
          </div>
        )}
        {accountsModal?.loading ? (
          <div className="text-sm text-zinc-500">Loading…</div>
        ) : accountsModal?.data && accountsModal.data.accounts.length === 0 ? (
          <div className="text-sm text-zinc-500">
            No accounts exist yet. Create accounts first.
          </div>
        ) : accountsModal?.data ? (
          <div className="max-h-72 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-800 divide-y divide-zinc-100 dark:divide-zinc-800">
            {accountsModal.data.accounts.map((name) => {
              const checked = accountsModal.selected.has(name);
              return (
                <label
                  key={name}
                  className="flex items-center gap-3 px-3 py-2 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900/50 cursor-pointer"
                >
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={() => toggleAccount(name)}
                    className="rounded"
                  />
                  <span className="font-mono flex-1">{name}</span>
                </label>
              );
            })}
          </div>
        ) : null}
      </Modal>
    </div>
  );
}
