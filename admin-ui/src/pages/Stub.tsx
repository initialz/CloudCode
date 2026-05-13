/** Placeholder pages — real implementations come in M3-M7. */
export function Stub({ title }: { title: string }) {
  return (
    <div className="text-zinc-500 text-sm">
      <h2 className="text-base font-semibold text-zinc-900 dark:text-zinc-100 mb-2">{title}</h2>
      <p>This view will land in a later milestone.</p>
    </div>
  );
}
