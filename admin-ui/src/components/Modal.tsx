import { useEffect, type ReactNode } from 'react';

interface Props {
  open: boolean;
  onClose: () => void;
  title: string;
  children: ReactNode;
  footer?: ReactNode;
}

export function Modal({ open, onClose, title, children, footer }: Props) {
  // Close on Escape.
  useEffect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') onClose();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, onClose]);

  if (!open) return null;
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-[2px]"
      onClick={onClose}
    >
      <div
        className="w-full max-w-md rounded-lg bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 shadow-xl"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <div className="border-b border-zinc-200 dark:border-zinc-800 px-4 py-3">
          <h3 className="font-semibold text-base">{title}</h3>
        </div>
        <div className="px-4 py-4 space-y-3">{children}</div>
        {footer && (
          <div className="border-t border-zinc-200 dark:border-zinc-800 px-4 py-3 flex justify-end gap-2">
            {footer}
          </div>
        )}
      </div>
    </div>
  );
}
