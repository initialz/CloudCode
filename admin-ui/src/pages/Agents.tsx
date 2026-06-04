import { useEffect, useState } from 'react';
import { apiClient, type AgentRowDto, type AllowedAccountsDto } from '@/lib/api';
import { Modal } from '@/components/Modal';
import { versionsEqual } from '@/lib/version';

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
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  // Per-agent in-flight update state. Drives the inline "Updating..."
  // animation. Failures are surfaced inline next to the button.
  const [updating, setUpdating] = useState<Record<string, true>>({});
  const [updateErrs, setUpdateErrs] = useState<Record<string, string>>({});
  const [dot, setDot] = useState(1);

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
    // Periodically refresh so a newly-published release surfaces as
    // an inline "Update to vX.Y.Z" button shortly after the tag goes
    // up, instead of only on page load. TEMP cadence — restore to
    // mount-only (or a slower poll) after the self-update test
    // cycle finishes.
    const t = window.setInterval(reload, 60_000);
    return () => window.clearInterval(t);
  }, []);

  // Dot animation while any agent is mid-update.
  useEffect(() => {
    if (Object.keys(updating).length === 0) return;
    const t = window.setInterval(() => setDot((d) => (d % 3) + 1), 500);
    return () => window.clearInterval(t);
  }, [updating]);

  async function handleUpdate(agent: AgentRowDto) {
    const target = agent.latest_version;
    if (!target) return;
    setUpdating((u) => ({ ...u, [agent.name]: true }));
    setUpdateErrs((e) => {
      const next = { ...e };
      delete next[agent.name];
      return next;
    });
    try {
      // Always update to the latest version that the hub knows
      // about. The previous per-version dropdown is gone — picking
      // older releases isn't a documented support path and made the
      // happy-path UX two clicks longer than necessary.
      await apiClient.agents.update(agent.name, target);
      // The update API returns after the agent has ACK'd that it
      // downloaded + installed; the agent then exits and the
      // supervisor re-execs. There's a short window where the
      // agent is offline / still booting the new binary — keep
      // polling the registry until the target row shows
      // `online && version == target`, or fall back after 90s.
      await waitForAgentVersion(agent.name, target);
      await reload();
    } catch (e: unknown) {
      const msg =
        typeof e === 'object' && e && 'message' in e
          ? String((e as { message?: unknown }).message ?? 'update failed')
          : 'update failed';
      setUpdateErrs((errs) => ({ ...errs, [agent.name]: msg }));
    } finally {
      setUpdating((u) => {
        const next = { ...u };
        delete next[agent.name];
        return next;
      });
    }
  }

  /// Poll agents.list() every 1.5 s until `name` reports
  /// `online && version >= target` (semver-equal counts) or until 90 s
  /// elapse. Rejects on timeout so the caller's catch surfaces an
  /// inline "failed" badge instead of silently flipping back to
  /// "Update to vX.Y.Z".
  async function waitForAgentVersion(name: string, target: string): Promise<void> {
    const deadline = Date.now() + 90_000;
    while (Date.now() < deadline) {
      try {
        const list = await apiClient.agents.list();
        const me = list.find((a) => a.name === name);
        if (me && me.online && versionsEqual(me.version, target)) {
          setAgents(list);
          return;
        }
        // Mid-flight: surface the in-flight list so the UI keeps
        // updating other fields (last_seen, etc.) without dropping
        // back to the "Update to ..." button.
        setAgents(list);
      } catch {
        // Hub flap or transient error — keep polling.
      }
      await new Promise((r) => setTimeout(r, 1500));
    }
    throw { message: `agent did not report ${target} within 90s` };
  }

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

  async function onDeleteAgent(name: string) {
    setDeleting(true);
    try {
      await apiClient.agents.delete(name);
      setConfirmDelete(null);
      await reload();
    } catch (e: any) {
      setErr(e?.message ?? 'delete failed');
    } finally {
      setDeleting(false);
    }
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
                <th className="px-3 py-2 text-left">Version</th>
                <th className="px-3 py-2 text-left">Accounts</th>
                <th className="px-3 py-2 text-right">Actions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-200 dark:divide-zinc-800 bg-white dark:bg-zinc-900">
              {agents.length === 0 ? (
                <tr>
                  <td colSpan={5} className="px-3 py-6 text-center text-zinc-500">
                    No agents have ever connected to this hub yet.
                  </td>
                </tr>
              ) : (
                agents.map((a) => {
                  const isUpToDate = versionsEqual(a.version, a.latest_version);
                  // `!= null` (loose) guards against BOTH null and undefined —
                  // if latest_version is ever absent from the response we must
                  // not fall through and render "Update to undefined".
                  const hasUpdate =
                    a.online &&
                    a.version != null &&
                    a.latest_version != null &&
                    !isUpToDate;
                  const isUpdating = updating[a.name] === true;
                  const updateErr = updateErrs[a.name];

                  let updateBtnLabel: React.ReactNode;
                  let updateBtnClass: string;
                  let updateBtnDisabled = false;
                  if (!a.online) {
                    updateBtnLabel = '—';
                    updateBtnClass = 'text-xs text-zinc-400';
                    updateBtnDisabled = true;
                  } else if (isUpdating) {
                    // Fixed-width dot box so the button doesn't reflow
                    // each 500 ms as the dot count cycles 1 → 2 → 3.
                    updateBtnLabel = (
                      <>
                        Updating
                        <span
                          className="inline-block text-left"
                          style={{ width: '1.5ch' }}
                          aria-hidden
                        >
                          {'.'.repeat(dot)}
                        </span>
                      </>
                    );
                    updateBtnClass =
                      'px-2 py-1 text-xs rounded border border-amber-300 dark:border-amber-700/50 text-amber-700 dark:text-amber-300 bg-amber-50 dark:bg-amber-900/20 select-none';
                    updateBtnDisabled = true;
                  } else if (hasUpdate) {
                    updateBtnLabel = `Update to ${a.latest_version}`;
                    updateBtnClass =
                      'px-2 py-1 text-xs rounded border border-red-300 dark:border-red-700/50 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-950/20';
                  } else if (isUpToDate) {
                    updateBtnLabel = 'Up to date';
                    updateBtnClass =
                      'px-2 py-1 text-xs rounded border border-zinc-300 dark:border-zinc-700 text-zinc-500 dark:text-zinc-400';
                    updateBtnDisabled = true;
                  } else {
                    // online but version unknown — can't safely target latest
                    updateBtnLabel = 'Version unknown';
                    updateBtnClass =
                      'px-2 py-1 text-xs rounded border border-zinc-300 dark:border-zinc-700 text-zinc-500 dark:text-zinc-400';
                    updateBtnDisabled = true;
                  }

                  return (
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
                        <span className="font-mono text-xs">{a.version ?? '—'}</span>
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
                      <td className="px-3 py-2 text-right whitespace-nowrap space-x-1">
                        {a.online ? (
                          <button
                            onClick={() => handleUpdate(a)}
                            disabled={updateBtnDisabled}
                            className={updateBtnClass}
                            title={updateErr ?? undefined}
                          >
                            {updateBtnLabel}
                          </button>
                        ) : (
                          <span className="text-xs text-zinc-400">—</span>
                        )}
                        {updateErr && (
                          <span
                            className="px-2 py-1 text-xs rounded bg-red-100 dark:bg-red-900/40 text-red-800 dark:text-red-200"
                            title={updateErr}
                          >
                            failed
                          </span>
                        )}
                        {a.online ? (
                          <span
                            className="text-xs text-zinc-400"
                            title="Online agents can't be deleted — disconnect on the agent host first"
                          >
                            {/* spacer */}
                          </span>
                        ) : (
                          <button
                            onClick={() => setConfirmDelete(a.name)}
                            className="px-2 py-1 text-xs rounded border border-red-300 dark:border-red-700/50 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-950/20"
                          >
                            Delete
                          </button>
                        )}
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

      <Modal
        open={confirmDelete !== null}
        onClose={() => !deleting && setConfirmDelete(null)}
        title={`Delete agent ${confirmDelete ?? ''}?`}
        footer={
          <>
            <button
              disabled={deleting}
              onClick={() => setConfirmDelete(null)}
              className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              disabled={deleting}
              onClick={() => confirmDelete && onDeleteAgent(confirmDelete)}
              className="px-3 py-1.5 text-sm rounded bg-red-600 text-white hover:bg-red-700 disabled:opacity-50"
            >
              {deleting ? 'Deleting…' : 'Delete'}
            </button>
          </>
        }
      >
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          Drops every ACL row that mentions this agent. If the same
          name comes back online later it will start with an empty
          allowlist. Session / audit history that already references
          this name is left untouched.
        </p>
      </Modal>
    </div>
  );
}
