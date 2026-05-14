import { useEffect, useState } from 'react';
import { apiClient, type AgentRowDto, type ReleaseDto } from '@/lib/api';
import { Modal } from '@/components/Modal';

// Compare semver tags like "v1.5.0". Returns -1 | 0 | 1.
function compareSemver(a: string, b: string): -1 | 0 | 1 {
  const parse = (s: string) => s.replace(/^v/, '').split('.').map(Number);
  const [aMaj, aMin, aPat] = parse(a);
  const [bMaj, bMin, bPat] = parse(b);
  for (const [x, y] of [
    [aMaj, bMaj],
    [aMin, bMin],
    [aPat, bPat],
  ] as [number, number][]) {
    if (x > y) return 1;
    if (x < y) return -1;
  }
  return 0;
}

interface Props {
  open: boolean;
  onClose: () => void;
  agent: AgentRowDto;
  onUpdated: () => void;
}

export function UpdateAgentModal({ open, onClose, agent, onUpdated }: Props) {
  const [releases, setReleases] = useState<ReleaseDto[]>([]);
  const [loading, setLoading] = useState(false);
  const [loadErr, setLoadErr] = useState<string | null>(null);

  const [target, setTarget] = useState<string>('');
  const [saving, setSaving] = useState(false);
  const [saveErr, setSaveErr] = useState<string | null>(null);
  const [successMsg, setSuccessMsg] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setReleases([]);
    setLoadErr(null);
    setSaveErr(null);
    setSuccessMsg(null);
    setSaving(false);
    setLoading(true);

    apiClient.agents
      .releases()
      .then((data) => {
        setReleases(data.releases);
        // Default: latest, or agent.version if present in list, fallback to first.
        const defaultTarget =
          data.latest ??
          (data.releases.length > 0 ? data.releases[0].tag : '');
        setTarget(defaultTarget);
      })
      .catch((e: any) => {
        setLoadErr(e?.message ?? 'Failed to load releases');
      })
      .finally(() => setLoading(false));
  }, [open]);

  async function onSubmit() {
    setSaveErr(null);
    setSuccessMsg(null);
    setSaving(true);
    try {
      await apiClient.agents.update(agent.name, target);
      setSuccessMsg(`Update queued — agent restarted onto ${target}.`);
      onUpdated();
    } catch (e: any) {
      setSaveErr(e?.message ?? 'Update failed');
    } finally {
      setSaving(false);
    }
  }

  // Determine submit button label.
  const cmp = agent.version && target ? compareSemver(target, agent.version) : null;
  let submitLabel: string;
  let submitDisabled: boolean;
  if (saving) {
    submitLabel = 'Updating…';
    submitDisabled = true;
  } else if (!target || loading) {
    submitLabel = 'Update';
    submitDisabled = true;
  } else if (target === agent.version) {
    submitLabel = 'Same as current';
    submitDisabled = true;
  } else if (cmp !== null && cmp < 0) {
    submitLabel = `Roll back to ${target}`;
    submitDisabled = false;
  } else {
    submitLabel = 'Update';
    submitDisabled = false;
  }

  return (
    <Modal
      open={open}
      onClose={() => !saving && onClose()}
      title={`Update agent ${agent.name}`}
      footer={
        <>
          <button
            disabled={saving}
            onClick={onClose}
            className="px-3 py-1.5 text-sm rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            disabled={submitDisabled}
            onClick={onSubmit}
            className="px-3 py-1.5 text-sm rounded bg-zinc-900 dark:bg-zinc-100 text-white dark:text-zinc-900 hover:opacity-90 disabled:opacity-50"
          >
            {submitLabel}
          </button>
        </>
      }
    >
      {loadErr && (
        <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
          {loadErr}
        </div>
      )}
      {saveErr && (
        <div className="text-sm rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-red-700 dark:text-red-300">
          {saveErr}
        </div>
      )}
      {successMsg && (
        <div className="text-sm rounded border-l-2 border-green-500 bg-green-50 dark:bg-green-950/30 px-3 py-2 text-green-700 dark:text-green-300">
          {successMsg}
        </div>
      )}

      <div className="text-sm text-zinc-600 dark:text-zinc-400">
        Current version:{' '}
        <span className="font-mono">{agent.version ?? '—'}</span>
      </div>

      <div className="space-y-1">
        <label className="text-sm font-medium text-zinc-700 dark:text-zinc-300">
          Target version
        </label>
        {loading ? (
          <div className="text-sm text-zinc-500">Loading releases…</div>
        ) : releases.length === 0 ? (
          <div className="text-sm text-zinc-500">No releases available.</div>
        ) : (
          <select
            value={target}
            onChange={(e) => setTarget(e.target.value)}
            disabled={saving}
            className="w-full px-3 py-2 rounded border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 text-sm focus:outline-none focus:ring-2 focus:ring-zinc-400 disabled:opacity-50"
          >
            {releases.map((r) => (
              <option key={r.tag} value={r.tag}>
                {r.tag}  ({r.date.slice(0, 10)})
              </option>
            ))}
          </select>
        )}
      </div>
    </Modal>
  );
}
