import { useState } from 'react';

export function CopyableToken({ token }: { token: string }) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(token);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      /* clipboard blocked; user can still select+copy manually */
    }
  }

  return (
    <div className="space-y-2">
      <div className="font-mono text-sm break-all rounded bg-zinc-950 text-zinc-100 px-3 py-2 select-all">
        {token}
      </div>
      <button
        onClick={copy}
        className="text-xs px-2 py-1 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
      >
        {copied ? '✓ Copied' : 'Copy'}
      </button>
    </div>
  );
}
