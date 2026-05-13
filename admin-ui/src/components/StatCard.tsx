import { Link } from 'react-router-dom';
import type { ReactNode } from 'react';

interface Props {
  label: string;
  value: ReactNode;
  sub?: ReactNode;
  link?: string;
}

export function StatCard({ label, value, sub, link }: Props) {
  const inner = (
    <>
      <div className="text-xs uppercase tracking-wide text-zinc-500">{label}</div>
      <div className="text-3xl font-semibold mt-1 tabular-nums">{value}</div>
      {sub !== undefined && (
        <div className="text-xs text-zinc-500 mt-1 truncate" title={typeof sub === 'string' ? sub : undefined}>
          {sub}
        </div>
      )}
    </>
  );
  const className =
    'block rounded-lg border border-zinc-200 dark:border-zinc-800 p-4 bg-white dark:bg-zinc-900 transition ' +
    (link ? 'hover:border-zinc-400 dark:hover:border-zinc-600' : '');
  return link ? (
    <Link to={link} className={className}>
      {inner}
    </Link>
  ) : (
    <div className={className}>{inner}</div>
  );
}
