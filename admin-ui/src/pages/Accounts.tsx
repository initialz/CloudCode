import { useEffect, useState } from 'react';
import { apiClient, type AccountDto } from '@/lib/api';
import { Modal } from '@/components/Modal';
import { CopyableToken } from '@/components/CopyableToken';

type TokenModal = { name: string; token: string; mode: 'created' | 'rotated' };

export function Accounts() {
  const [accounts, setAccounts] = useState<AccountDto[] | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState('');
  const [createErr, setCreateErr] = useState<string | null>(null);

  const [tokenModal, setTokenModal] = useState<TokenModal | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  async function reload() {
    try {
      const list = await apiClient.accounts.list();
      setAccounts(list);
    } catch (e: any) {
      setErr(e?.message ?? 'failed to load accounts');
    }
  }

  useEffect(() => {
    reload();
  }, []);

  async function onCreate() {
    setCreateErr(null);
    setPending(true);
    try {
      const r = await apiClient.accounts.create(newName.trim());
      setNewName('');
      setCreating(false);
      setTokenModal({ name: r.name, token: r.token, mode: 'created' });
      await reload();
    } catch (e: any) {
      setCreateErr(e?.message ?? 'create failed');
    } finally {
      setPending(false);
    }
  }

  async function onRotate(name: string) {
    setPending(true);
    try {
      const r = await apiClient.accounts.rotate(name);
      setTokenModal({ name: r.name, token: r.token, mode: 'rotated' });
      await reload();
    } catch (e: any) {
      setErr(e?.message ?? 'rotate failed');
    } finally {
      setPending(false);
    }
  }

  async function onToggle(name: string) {
    setPending(true);
    try {
      await apiClient.accounts.toggle(name);
      await reload();
    } catch (e: any) {
      setErr(e?.message ?? 'toggle failed');
    } finally {
      setPending(false);
    }
  }

  async function onDelete(name: string) {
    setPending(true);
    try {
      await apiClient.accounts.delete(name);
      setConfirmDelete(null);
      await reload();
    } catch (e: any) {
      setErr(e?.message ?? 'delete failed');
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-base font-semibold">Accounts</h2>
        <button
          onClick={() => {
            setCreating(true);
            setCreateErr(null);
            setNewName('');
          }}
          className="px-3 py-1.5 rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 text-sm hover:opacity-90"
        >
          + New account
        </button>
      </div>

      {err && (
        <div className="rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-sm text-red-700 dark:text-red-300">
          {err}
        </div>
      )}

      {accounts === null ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-50 dark:bg-zinc-900/50 text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2 text-left">Name</th>
                <th className="px-3 py-2 text-left">Token suffix</th>
                <th className="px-3 py-2 text-left">Status</th>
                <th className="px-3 py-2 text-left">Created</th>
                <th className="px-3 py-2 text-right">Actions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-200 dark:divide-zinc-800 bg-white dark:bg-zinc-900">
              {accounts.length === 0 ? (
                <tr>
                  <td colSpan={5} className="px-3 py-6 text-center text-zinc-500">
                    No accounts yet. Create one above.
                  </td>
                </tr>
              ) : (
                accounts.map((a) => (
                  <tr key={a.name}>
                    <td className="px-3 py-2 font-mono">{a.name}</td>
                    <td className="px-3 py-2 font-mono text-zinc-500">
                      …{a.token_prefix ?? <span className="italic">legacy</span>}
                    </td>
                    <td className="px-3 py-2">
                      {a.disabled ? (
                        <span className="text-xs px-2 py-0.5 rounded bg-zinc-200 dark:bg-zinc-800 text-zinc-600 dark:text-zinc-400">
                          disabled
                        </span>
                      ) : (
                        <span className="text-xs px-2 py-0.5 rounded bg-green-100 dark:bg-green-900/40 text-green-700 dark:text-green-300">
                          active
                        </span>
                      )}
                    </td>
                    <td className="px-3 py-2 text-zinc-500">
                      {new Date(a.created_at * 1000).toISOString().slice(0, 10)}
                    </td>
                    <td className="px-3 py-2 text-right space-x-1 whitespace-nowrap">
                      <button
                        disabled={pending}
                        onClick={() => onRotate(a.name)}
                        className="px-2 py-1 text-xs rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
                      >
                        Rotate
                      </button>
                      <button
                        disabled={pending}
                        onClick={() => onToggle(a.name)}
                        className="px-2 py-1 text-xs rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
                      >
                        {a.disabled ? 'Enable' : 'Disable'}
                      </button>
                      <button
                        disabled={pending}
                        onClick={() => setConfirmDelete(a.name)}
                        className="px-2 py-1 text-xs rounded border border-red-300 dark:border-red-700/50 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-950/20 disabled:opacity-50"
                      >
                        Delete
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
        open={creating}
        onClose={() => setCreating(false)}
        title="New account"
        footer={
          <>
            <button
              onClick={() => setCreating(false)}
              className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
            >
              Cancel
            </button>
            <button
              disabled={pending || !newName.trim()}
              onClick={onCreate}
              className="px-3 py-1.5 text-sm rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90 disabled:opacity-50"
            >
              Create
            </button>
          </>
        }
      >
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          Letters, digits, <code>_</code> or <code>-</code> (max 64 chars).
        </p>
        {createErr && (
          <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
            {createErr}
          </div>
        )}
        <input
          autoFocus
          value={newName}
          onChange={(e) => setNewName(e.target.value)}
          placeholder="alice"
          className="w-full px-3 py-2 rounded border border-zinc-300 dark:border-zinc-700 bg-transparent text-sm focus:outline-none focus:ring-2 focus:ring-zinc-400"
        />
      </Modal>

      <Modal
        open={tokenModal !== null}
        onClose={() => setTokenModal(null)}
        title={
          tokenModal?.mode === 'rotated'
            ? `Token rotated for ${tokenModal?.name}`
            : `Account ${tokenModal?.name ?? ''} created`
        }
        footer={
          <button
            onClick={() => setTokenModal(null)}
            className="px-3 py-1.5 text-sm rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90"
          >
            Done
          </button>
        }
      >
        <p className="text-sm rounded border-l-2 border-yellow-500 bg-yellow-50 dark:bg-yellow-950/30 px-3 py-2 text-yellow-800 dark:text-yellow-200">
          This token is shown only once. Copy it before closing this dialog.
        </p>
        {tokenModal && <CopyableToken token={tokenModal.token} />}
      </Modal>

      <Modal
        open={confirmDelete !== null}
        onClose={() => setConfirmDelete(null)}
        title={`Delete account ${confirmDelete ?? ''}?`}
        footer={
          <>
            <button
              onClick={() => setConfirmDelete(null)}
              className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
            >
              Cancel
            </button>
            <button
              disabled={pending}
              onClick={() => confirmDelete && onDelete(confirmDelete)}
              className="px-3 py-1.5 text-sm rounded bg-red-600 text-white hover:bg-red-700 disabled:opacity-50"
            >
              Delete
            </button>
          </>
        }
      >
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          The existing token stops working immediately and the row is removed. This cannot be undone.
        </p>
      </Modal>
    </div>
  );
}
