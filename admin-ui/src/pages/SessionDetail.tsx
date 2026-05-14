import { useEffect, useState, type ReactNode } from 'react';
import { Link, useParams } from 'react-router-dom';
import { apiClient, type MessageDto, type SessionDetailDto } from '@/lib/api';
import { formatDateTime } from '@/lib/time';

export function SessionDetail() {
  const { id } = useParams<{ id: string }>();
  const [detail, setDetail] = useState<SessionDetailDto | null>(null);
  const [messages, setMessages] = useState<MessageDto[] | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    setDetail(null);
    setMessages(null);
    setErr(null);
    Promise.all([apiClient.sessions.detail(id), apiClient.sessions.messages(id)])
      .then(([d, m]) => {
        if (cancelled) return;
        setDetail(d);
        setMessages(m);
      })
      .catch((e: any) => {
        if (cancelled) return;
        setErr(e?.message ?? 'load failed');
      });
    return () => {
      cancelled = true;
    };
  }, [id]);

  if (err) {
    return (
      <div className="rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-sm text-red-700 dark:text-red-300">
        {err}
      </div>
    );
  }
  if (!detail || !messages) {
    return <div className="text-sm text-zinc-500">Loading…</div>;
  }

  return (
    <div className="space-y-6">
      <header className="space-y-2">
        <Link to="/sessions" className="text-xs text-zinc-500 hover:underline">
          ← back to sessions
        </Link>
        <h2 className="text-base font-semibold">
          Session <code>{detail.session_id.slice(0, 12)}…</code>
        </h2>
        <dl className="grid grid-cols-[6rem_1fr] text-xs gap-y-1 text-zinc-500">
          <Pair k="account" v={detail.account} />
          <Pair k="agent" v={detail.agent} />
          <Pair k="workspace" v={detail.workspace} />
          <Pair k="started" v={formatTs(detail.started_at)} />
          {detail.ended_at ? (
            <Pair
              k="ended"
              v={`${formatTs(detail.ended_at)} (${formatDuration(detail.ended_at - detail.started_at)})${
                detail.ended_reason ? ` — ${detail.ended_reason}` : ''
              }`}
            />
          ) : (
            <Pair k="status" v={<span className="text-green-600 dark:text-green-400">● live</span>} />
          )}
          <Pair k="messages" v={String(detail.message_count)} />
        </dl>
      </header>

      <div className="space-y-3">
        {messages.length === 0 ? (
          <div className="rounded border border-dashed border-zinc-300 dark:border-zinc-700 p-6 text-center text-sm text-zinc-500">
            No messages tailed for this session yet. The agent watches{' '}
            <code>~/.claude/projects/&lt;workspace&gt;/</code> on the host and ships new lines here
            in real time.
          </div>
        ) : (
          messages.map((m) => <MessageView key={m.id} msg={m} />)
        )}
      </div>
    </div>
  );
}

function MessageView({ msg }: { msg: MessageDto }) {
  if (msg.kind === 'user') {
    const inner = msg.body?.message?.content;
    const text =
      typeof inner === 'string'
        ? inner
        : Array.isArray(inner)
        ? inner.map(blockToString).join('\n')
        : JSON.stringify(inner ?? '');
    return (
      <div className="rounded border bg-blue-50 dark:bg-blue-950/30 border-blue-200 dark:border-blue-900/50 p-3 max-w-3xl">
        <div className="text-xs text-zinc-500 mb-1">user · {formatTs(msg.ts)}</div>
        <div className="text-sm whitespace-pre-wrap break-words">{text}</div>
      </div>
    );
  }

  if (msg.kind === 'assistant') {
    const content = msg.body?.message?.content;
    const blocks: any[] = Array.isArray(content) ? content : [{ type: 'text', text: String(content ?? '') }];
    return (
      <div className="rounded border bg-zinc-100 dark:bg-zinc-800/60 border-zinc-200 dark:border-zinc-700 p-3 max-w-3xl ml-8 space-y-2">
        <div className="text-xs text-zinc-500">assistant · {formatTs(msg.ts)}</div>
        {blocks.map((b, i) => (
          <Block key={i} block={b} />
        ))}
      </div>
    );
  }

  if (msg.kind === 'summary' && typeof msg.body?.summary === 'string') {
    return (
      <div className="text-xs text-zinc-500 italic border-l-2 border-zinc-300 dark:border-zinc-700 pl-3 py-1">
        summary · {formatTs(msg.ts)} — {msg.body.summary}
      </div>
    );
  }

  return (
    <details className="text-xs text-zinc-500 border-l-2 border-zinc-200 dark:border-zinc-800 pl-3">
      <summary className="cursor-pointer">
        {msg.kind} · {formatTs(msg.ts)}
      </summary>
      <pre className="mt-1 p-2 bg-zinc-950 text-zinc-100 rounded overflow-x-auto text-[11px]">
        {JSON.stringify(msg.body, null, 2)}
      </pre>
    </details>
  );
}

function Block({ block }: { block: any }) {
  if (!block || typeof block !== 'object') {
    return <pre className="text-xs">{String(block)}</pre>;
  }
  if (block.type === 'text') {
    return <div className="text-sm whitespace-pre-wrap break-words">{String(block.text ?? '')}</div>;
  }
  if (block.type === 'thinking') {
    return (
      <details className="text-xs">
        <summary className="cursor-pointer italic text-zinc-500">💭 thinking</summary>
        <div className="mt-1 text-zinc-500 italic whitespace-pre-wrap break-words pl-3">
          {String(block.text ?? '')}
        </div>
      </details>
    );
  }
  if (block.type === 'tool_use') {
    return (
      <details className="text-xs">
        <summary className="cursor-pointer text-zinc-500">
          🔧 <code className="text-zinc-900 dark:text-zinc-100">{block.name}</code>
        </summary>
        <pre className="mt-1 p-2 bg-zinc-950 text-zinc-100 rounded overflow-x-auto text-[11px]">
          {JSON.stringify(block.input ?? {}, null, 2)}
        </pre>
      </details>
    );
  }
  if (block.type === 'tool_result') {
    const r =
      typeof block.content === 'string'
        ? block.content
        : Array.isArray(block.content)
        ? block.content.map(blockToString).join('\n')
        : JSON.stringify(block.content ?? '');
    return (
      <details className="text-xs">
        <summary className="cursor-pointer text-zinc-500">✓ tool_result</summary>
        <pre className="mt-1 p-2 bg-zinc-950 text-zinc-100 rounded overflow-x-auto text-[11px] whitespace-pre-wrap break-words">
          {r}
        </pre>
      </details>
    );
  }
  return (
    <pre className="text-[11px] text-zinc-500 overflow-x-auto">
      {JSON.stringify(block, null, 2)}
    </pre>
  );
}

function blockToString(b: any): string {
  if (b && typeof b === 'object' && typeof b.text === 'string') return b.text;
  return JSON.stringify(b);
}

function Pair({ k, v }: { k: string; v: ReactNode }) {
  return (
    <>
      <dt>{k}</dt>
      <dd>{typeof v === 'string' ? <code className="text-zinc-900 dark:text-zinc-100">{v}</code> : v}</dd>
    </>
  );
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
