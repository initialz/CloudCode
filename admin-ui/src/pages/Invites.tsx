import { useEffect, useState } from 'react';
import {
  apiClient,
  type AgentRowDto,
  type InviteAcceptanceDto,
  type InviteDto,
} from '@/lib/api';
import { Modal } from '@/components/Modal';
import { formatDate, formatRelative } from '@/lib/time';
import { resolveHubUrl } from '@/lib/hubUrl';

/** Build the public invite URL using the configured hub URL setting.
 *  Backend's share_url is ignored — it's built from the admin host which
 *  is typically a different port than the public webterm. */
function inviteShareUrl(token: string): string {
  const base = resolveHubUrl().replace(/\/$/, '');
  return `${base}/invite/${encodeURIComponent(token)}`;
}

type CreatedInvite = {
  id: string;
  token: string;
  share_url: string;
};

type AcceptancesModalState = {
  invite: InviteDto;
  data: InviteAcceptanceDto[] | null;
  loading: boolean;
  err: string | null;
};

export function Invites() {
  const [invites, setInvites] = useState<InviteDto[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  // Create modal state
  const [creating, setCreating] = useState(false);
  const [newLabel, setNewLabel] = useState('');
  const [newMaxUses, setNewMaxUses] = useState('0');
  const [newAgents, setNewAgents] = useState<Set<string>>(new Set());
  const [agentsPool, setAgentsPool] = useState<AgentRowDto[] | null>(null);
  const [agentsErr, setAgentsErr] = useState<string | null>(null);
  const [createErr, setCreateErr] = useState<string | null>(null);

  // After-create modal showing the share URL
  const [createdInvite, setCreatedInvite] = useState<CreatedInvite | null>(null);

  // Confirm delete
  const [confirmDelete, setConfirmDelete] = useState<InviteDto | null>(null);

  // Acceptances modal
  const [acceptancesModal, setAcceptancesModal] =
    useState<AcceptancesModalState | null>(null);

  // Copy feedback per invite row
  const [copiedId, setCopiedId] = useState<string | null>(null);
  // Copy feedback inside created-invite modal
  const [createdCopied, setCreatedCopied] = useState(false);

  async function reload() {
    try {
      const list = await apiClient.invites.list();
      setInvites(list);
    } catch (e: any) {
      setErr(e?.message ?? 'failed to load invites');
    }
  }

  useEffect(() => {
    reload();
  }, []);

  async function loadAgents() {
    setAgentsErr(null);
    try {
      const list = await apiClient.agents.list();
      setAgentsPool(list);
    } catch (e: any) {
      setAgentsErr(e?.message ?? 'failed to load agents');
    }
  }

  function openCreate() {
    setCreating(true);
    setNewLabel('');
    setNewMaxUses('0');
    setNewAgents(new Set());
    setCreateErr(null);
    setAgentsPool(null);
    loadAgents();
  }

  function toggleNewAgent(name: string) {
    setNewAgents((cur) => {
      const next = new Set(cur);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  }

  async function onCreate() {
    setCreateErr(null);
    const max = Number.parseInt(newMaxUses, 10);
    if (Number.isNaN(max) || max < 0) {
      setCreateErr('Max uses must be a non-negative integer (0 = unlimited).');
      return;
    }
    setPending(true);
    try {
      const label = newLabel.trim();
      const r = await apiClient.invites.create({
        ...(label ? { label } : {}),
        max_uses: max,
        allowed_agents: Array.from(newAgents).sort(),
      });
      setCreating(false);
      setCreatedInvite(r);
      setCreatedCopied(false);
      await reload();
    } catch (e: any) {
      setCreateErr(e?.message ?? 'create failed');
    } finally {
      setPending(false);
    }
  }

  async function onToggleActive(inv: InviteDto) {
    setPending(true);
    try {
      await apiClient.invites.setActive(inv.id, !inv.active);
      await reload();
    } catch (e: any) {
      setErr(e?.message ?? 'toggle failed');
    } finally {
      setPending(false);
    }
  }

  async function onDelete(id: string) {
    setPending(true);
    try {
      await apiClient.invites.delete(id);
      setConfirmDelete(null);
      await reload();
    } catch (e: any) {
      setErr(e?.message ?? 'delete failed');
    } finally {
      setPending(false);
    }
  }

  async function copyShareUrl(inv: InviteDto) {
    try {
      await navigator.clipboard.writeText(inviteShareUrl(inv.token));
      setCopiedId(inv.id);
      setTimeout(
        () => setCopiedId((cur) => (cur === inv.id ? null : cur)),
        2000,
      );
    } catch {
      /* clipboard blocked */
    }
  }

  async function copyCreatedShareUrl() {
    if (!createdInvite) return;
    try {
      await navigator.clipboard.writeText(inviteShareUrl(createdInvite.token));
      setCreatedCopied(true);
      setTimeout(() => setCreatedCopied(false), 2000);
    } catch {
      /* ignored */
    }
  }

  async function openAcceptances(inv: InviteDto) {
    setAcceptancesModal({ invite: inv, data: null, loading: true, err: null });
    try {
      const data = await apiClient.invites.acceptances(inv.id);
      setAcceptancesModal((cur) =>
        cur && cur.invite.id === inv.id
          ? { ...cur, data, loading: false }
          : cur,
      );
    } catch (e: any) {
      setAcceptancesModal((cur) =>
        cur && cur.invite.id === inv.id
          ? { ...cur, loading: false, err: e?.message ?? 'failed to load' }
          : cur,
      );
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-base font-semibold">Invites</h2>
        <button
          onClick={openCreate}
          className="px-3 py-1.5 rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 text-sm hover:opacity-90"
        >
          + Create invite
        </button>
      </div>

      {err && (
        <div className="rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-sm text-red-700 dark:text-red-300">
          {err}
        </div>
      )}

      {invites === null ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-50 dark:bg-zinc-900/50 text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2 text-left">Label</th>
                <th className="px-3 py-2 text-left">Status</th>
                <th className="px-3 py-2 text-left">Used</th>
                <th className="px-3 py-2 text-left">Allowed agents</th>
                <th className="px-3 py-2 text-left">Created</th>
                <th className="px-3 py-2 text-right">Actions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-200 dark:divide-zinc-800 bg-white dark:bg-zinc-900">
              {invites.length === 0 ? (
                <tr>
                  <td
                    colSpan={6}
                    className="px-3 py-6 text-center text-zinc-500"
                  >
                    No invites yet. Create one above.
                  </td>
                </tr>
              ) : (
                invites.map((inv) => {
                  const limitLabel =
                    inv.max_uses === 0 ? '∞' : String(inv.max_uses);
                  return (
                    <tr key={inv.id}>
                      <td className="px-3 py-2 text-zinc-700 dark:text-zinc-200">
                        {inv.label ? (
                          inv.label
                        ) : (
                          <span className="text-zinc-400 italic">
                            (unnamed)
                          </span>
                        )}
                      </td>
                      <td className="px-3 py-2">
                        <button
                          disabled={pending}
                          onClick={() => onToggleActive(inv)}
                          title={
                            inv.active
                              ? 'Click to deactivate'
                              : 'Click to activate'
                          }
                          className={`text-xs px-2 py-0.5 rounded transition-colors disabled:opacity-50 ${
                            inv.active
                              ? 'bg-green-100 dark:bg-green-900/40 text-green-700 dark:text-green-300 hover:bg-green-200 dark:hover:bg-green-900/60'
                              : 'bg-zinc-200 dark:bg-zinc-800 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-300 dark:hover:bg-zinc-700'
                          }`}
                        >
                          {inv.active ? 'active' : 'inactive'}
                        </button>
                      </td>
                      <td className="px-3 py-2 font-mono text-zinc-700 dark:text-zinc-300">
                        {inv.used > 0 ? (
                          <button
                            type="button"
                            onClick={() => openAcceptances(inv)}
                            className="text-blue-600 dark:text-blue-400 hover:underline"
                            title="View accepted accounts"
                          >
                            {inv.used}
                          </button>
                        ) : (
                          inv.used
                        )}
                        {' / '}{limitLabel}
                      </td>
                      <td className="px-3 py-2">
                        {inv.allowed_agents.length === 0 ? (
                          <span
                            className="text-xs px-2 py-0.5 rounded bg-red-100 dark:bg-red-900/40 text-red-700 dark:text-red-300"
                            title="No agents are granted — accounts joining via this invite will not be able to connect to any agent"
                          >
                            (none)
                          </span>
                        ) : (
                          <div className="flex flex-wrap gap-1">
                            {inv.allowed_agents.map((name) => (
                              <span
                                key={name}
                                className="text-xs font-mono px-1.5 py-0.5 rounded bg-zinc-100 dark:bg-zinc-800 text-zinc-700 dark:text-zinc-300"
                              >
                                {name}
                              </span>
                            ))}
                          </div>
                        )}
                      </td>
                      <td className="px-3 py-2 text-zinc-500">
                        {formatRelative(inv.created_at)}
                      </td>
                      <td className="px-3 py-2 text-right space-x-1 whitespace-nowrap">
                        <button
                          onClick={() => copyShareUrl(inv)}
                          className="px-2 py-1 text-xs rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                        >
                          {copiedId === inv.id ? '✓ Copied' : 'Copy link'}
                        </button>
                        <button
                          disabled={pending}
                          onClick={() => onToggleActive(inv)}
                          className="px-2 py-1 text-xs rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
                        >
                          {inv.active ? 'Disable' : 'Enable'}
                        </button>
                        <button
                          disabled={pending}
                          onClick={() => setConfirmDelete(inv)}
                          className="px-2 py-1 text-xs rounded border border-red-300 dark:border-red-700/50 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-950/20 disabled:opacity-50"
                        >
                          Delete
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
        open={creating}
        onClose={() => !pending && setCreating(false)}
        title="New invite"
        footer={
          <>
            <button
              disabled={pending}
              onClick={() => setCreating(false)}
              className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              disabled={pending}
              onClick={onCreate}
              className="px-3 py-1.5 text-sm rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90 disabled:opacity-50"
            >
              {pending ? 'Creating…' : 'Create'}
            </button>
          </>
        }
      >
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          Anyone with the share link can claim a new account. Pick the agents
          that newly-joined accounts may use.
        </p>
        {createErr && (
          <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
            {createErr}
          </div>
        )}
        <div className="space-y-1">
          <label className="text-xs text-zinc-500">Label (optional)</label>
          <input
            autoFocus
            value={newLabel}
            onChange={(e) => setNewLabel(e.target.value)}
            placeholder="e.g. October cohort"
            className="w-full px-3 py-2 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent text-sm focus:outline-none focus:ring-2 focus:ring-zinc-400"
          />
        </div>
        <div className="space-y-1">
          <label className="text-xs text-zinc-500">
            Max uses{' '}
            <span className="text-zinc-400">(0 = unlimited)</span>
          </label>
          <input
            type="number"
            min={0}
            value={newMaxUses}
            onChange={(e) => setNewMaxUses(e.target.value)}
            className="w-full px-3 py-2 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent text-sm focus:outline-none focus:ring-2 focus:ring-zinc-400"
          />
        </div>
        <div className="space-y-1">
          <label className="text-xs text-zinc-500">Allowed agents</label>
          {agentsErr ? (
            <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
              {agentsErr}
            </div>
          ) : agentsPool === null ? (
            <div className="text-sm text-zinc-500">Loading agents…</div>
          ) : agentsPool.length === 0 ? (
            <div className="text-sm text-zinc-500">
              No agents have ever connected to this hub. Wait until at least
              one agent is online before creating an invite.
            </div>
          ) : (
            <div className="max-h-56 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-800 divide-y divide-zinc-100 dark:divide-zinc-800">
              {agentsPool.map((a) => {
                const checked = newAgents.has(a.name);
                return (
                  <label
                    key={a.name}
                    className="flex items-center gap-3 px-3 py-2 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900/50 cursor-pointer"
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      onChange={() => toggleNewAgent(a.name)}
                      className="rounded"
                    />
                    <span className="font-mono flex-1">{a.name}</span>
                    {a.online ? (
                      <span className="text-xs px-2 py-0.5 rounded bg-green-100 dark:bg-green-900/40 text-green-700 dark:text-green-300">
                        online
                      </span>
                    ) : (
                      <span className="text-xs px-2 py-0.5 rounded bg-zinc-100 dark:bg-zinc-800 text-zinc-500">
                        offline
                      </span>
                    )}
                  </label>
                );
              })}
            </div>
          )}
        </div>
      </Modal>

      <Modal
        open={createdInvite !== null}
        onClose={() => setCreatedInvite(null)}
        title="Invite created"
        footer={
          <button
            onClick={() => setCreatedInvite(null)}
            className="px-3 py-1.5 text-sm rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90"
          >
            Done
          </button>
        }
      >
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          Share this link with the people you want to onboard. Each visitor
          claims a fresh account; the link stays valid until deactivated or
          its max-use limit is reached.
        </p>
        {createdInvite && (
          <div className="space-y-1">
            <div className="text-xs text-zinc-500">Share link</div>
            <div className="font-mono text-sm break-all rounded bg-zinc-950 text-zinc-100 px-3 py-2 select-all">
              {inviteShareUrl(createdInvite.token)}
            </div>
            <button
              onClick={copyCreatedShareUrl}
              className="text-xs px-2 py-1 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
            >
              {createdCopied ? '✓ Copied' : 'Copy link'}
            </button>
          </div>
        )}
      </Modal>

      <Modal
        open={acceptancesModal !== null}
        onClose={() => setAcceptancesModal(null)}
        title={
          acceptancesModal
            ? `Acceptances — ${acceptancesModal.invite.label ?? '(unnamed)'}`
            : 'Acceptances'
        }
        footer={
          <button
            onClick={() => setAcceptancesModal(null)}
            className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
          >
            Close
          </button>
        }
      >
        {acceptancesModal?.err && (
          <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
            {acceptancesModal.err}
          </div>
        )}
        {acceptancesModal?.loading ? (
          <div className="text-sm text-zinc-500">Loading…</div>
        ) : acceptancesModal?.data && acceptancesModal.data.length === 0 ? (
          <div className="text-sm text-zinc-500">
            No one has accepted this invite yet.
          </div>
        ) : acceptancesModal?.data ? (
          <div className="max-h-72 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-800 divide-y divide-zinc-100 dark:divide-zinc-800">
            {acceptancesModal.data.map((row, idx) => (
              <div
                key={`${row.account}-${idx}`}
                className="flex items-center gap-3 px-3 py-2 text-sm"
              >
                <span className="font-mono">{row.account}</span>
                {row.real_name ? (
                  <span className="text-zinc-500">({row.real_name})</span>
                ) : null}
                <span
                  className="ml-auto text-xs text-zinc-500"
                  title={formatDate(row.accepted_at)}
                >
                  {formatRelative(row.accepted_at)}
                </span>
              </div>
            ))}
          </div>
        ) : null}
      </Modal>

      <Modal
        open={confirmDelete !== null}
        onClose={() => !pending && setConfirmDelete(null)}
        title={`Delete invite${
          confirmDelete?.label ? ` "${confirmDelete.label}"` : ''
        }?`}
        footer={
          <>
            <button
              disabled={pending}
              onClick={() => setConfirmDelete(null)}
              className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              disabled={pending}
              onClick={() => confirmDelete && onDelete(confirmDelete.id)}
              className="px-3 py-1.5 text-sm rounded bg-red-600 text-white hover:bg-red-700 disabled:opacity-50"
            >
              Delete
            </button>
          </>
        }
      >
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          The share link stops working immediately. Accounts that already
          accepted this invite are not affected. This cannot be undone.
        </p>
      </Modal>
    </div>
  );
}
