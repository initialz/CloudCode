import { useState } from 'react';
import { resolveHubUrl } from '@/lib/hubUrl';

export function CopyableToken({ token }: { token: string }) {
  const [copied, setCopied] = useState<'token' | 'install' | null>(null);
  const hubUrl = resolveHubUrl();

  const installCmd =
    `curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client \\\n` +
    `  && cloudcode --hub-url ${hubUrl} --token ${token}`;

  async function copyText(s: string, which: 'token' | 'install') {
    try {
      await navigator.clipboard.writeText(s);
      setCopied(which);
      setTimeout(() => setCopied(null), 2000);
    } catch {
      /* clipboard blocked; user can still select+copy manually */
    }
  }

  return (
    <div className="space-y-4">
      {/* token */}
      <div className="space-y-1">
        <div className="text-xs text-zinc-500">Account token (shown only once)</div>
        <div className="font-mono text-sm break-all rounded bg-zinc-950 text-zinc-100 px-3 py-2 select-all">
          {token}
        </div>
        <button
          onClick={() => copyText(token, 'token')}
          className="text-xs px-2 py-1 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
        >
          {copied === 'token' ? '✓ Copied' : 'Copy token'}
        </button>
      </div>

      {/* one-liner */}
      <div className="space-y-1">
        <div className="text-xs text-zinc-500">
          One-liner: install client and connect (hub URL from{' '}
          <span className="font-mono">Settings</span>)
        </div>
        <pre className="font-mono text-xs break-all whitespace-pre-wrap rounded bg-zinc-950 text-zinc-100 px-3 py-2 select-all">
{installCmd}
        </pre>
        <button
          onClick={() => copyText(installCmd, 'install')}
          className="text-xs px-2 py-1 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
        >
          {copied === 'install' ? '✓ Copied' : 'Copy command'}
        </button>
      </div>
    </div>
  );
}
