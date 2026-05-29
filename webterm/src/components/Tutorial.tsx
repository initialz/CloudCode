// First-time tour overlay. Highlights key UI areas by querying
// `[data-tutorial="<key>"]` elements, drawing a cutout around them
// and showing a popover next to them. Steps without a target render
// as a centered card.

import { useEffect, useState } from 'react';

type Step = {
  /** data-tutorial attribute on the target element. Omit for centered cards. */
  target?: string;
  title: string;
  body: string;
  /** Where the popover sits relative to the target. */
  placement?: 'right' | 'bottom' | 'top' | 'left';
};

const STEPS: Step[] = [
  {
    title: 'Welcome to CloudCode',
    body:
      "Let's take 30 seconds to show you around. You can skip anytime.",
  },
  {
    target: 'new-workspace',
    placement: 'right',
    title: 'Create your first workspace',
    body:
      "A workspace is your project folder. Click '+ New workspace', pick an agent, give it a name, and CloudCode boots tmux + claude inside.",
  },
  {
    target: 'workspace-list',
    placement: 'right',
    title: 'Your workspaces',
    body:
      'All your workspaces are listed here. Click one to attach to its terminal. You can have multiple workspaces open as tabs.',
  },
  {
    target: 'workspace-list',
    placement: 'right',
    title: 'Right-click for actions',
    body:
      'Right-click any workspace for Files (upload/download), Reset (kill session, keep code) and Delete.',
  },
  {
    target: 'settings',
    placement: 'top',
    title: 'Settings',
    body:
      'Click "settings" to set your real name, theme, and default tool arguments. Your settings are saved per account.',
  },
  {
    title: "You're all set",
    body:
      'Create a workspace from the left to get started. You can re-open this tour from settings later.',
  },
];

const LS_KEY = 'cc_tutorial_seen_v1';

export function hasSeenTutorial(): boolean {
  try {
    return localStorage.getItem(LS_KEY) === '1';
  } catch {
    return true;
  }
}

export function markTutorialSeen(): void {
  try {
    localStorage.setItem(LS_KEY, '1');
  } catch {
    /* ignore */
  }
}

export function clearTutorialSeen(): void {
  try {
    localStorage.removeItem(LS_KEY);
  } catch {
    /* ignore */
  }
}

type Props = { onClose: () => void };

export default function Tutorial({ onClose }: Props) {
  const [stepIndex, setStepIndex] = useState(0);
  const [rect, setRect] = useState<DOMRect | null>(null);
  const step = STEPS[stepIndex];

  // Locate the target element on every step change + window resize.
  useEffect(() => {
    function locate() {
      if (!step.target) {
        setRect(null);
        return;
      }
      const el = document.querySelector(
        `[data-tutorial="${step.target}"]`,
      ) as HTMLElement | null;
      if (!el) {
        setRect(null);
        return;
      }
      setRect(el.getBoundingClientRect());
    }
    locate();
    window.addEventListener('resize', locate);
    const t = window.setInterval(locate, 500); // catch layout shifts
    return () => {
      window.removeEventListener('resize', locate);
      window.clearInterval(t);
    };
  }, [step.target]);

  // Close on Escape
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') finish();
      if (e.key === 'ArrowRight' || e.key === 'Enter') next();
      if (e.key === 'ArrowLeft') prev();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stepIndex]);

  function next() {
    if (stepIndex >= STEPS.length - 1) finish();
    else setStepIndex(stepIndex + 1);
  }
  function prev() {
    if (stepIndex > 0) setStepIndex(stepIndex - 1);
  }
  function finish() {
    markTutorialSeen();
    onClose();
  }

  const isLast = stepIndex === STEPS.length - 1;
  const isFirst = stepIndex === 0;

  // ── Compute popover position
  const padding = 8;
  const popWidth = 320;
  let popStyle: React.CSSProperties = {};
  if (rect) {
    const placement = step.placement ?? 'right';
    if (placement === 'right') {
      popStyle = {
        left: Math.min(rect.right + padding, window.innerWidth - popWidth - padding),
        top: Math.max(padding, rect.top),
      };
    } else if (placement === 'bottom') {
      popStyle = {
        left: Math.max(
          padding,
          Math.min(rect.left, window.innerWidth - popWidth - padding),
        ),
        top: rect.bottom + padding,
      };
    } else if (placement === 'top') {
      popStyle = {
        left: Math.max(
          padding,
          Math.min(rect.left, window.innerWidth - popWidth - padding),
        ),
        bottom: window.innerHeight - rect.top + padding,
      };
    } else {
      popStyle = {
        right: window.innerWidth - rect.left + padding,
        top: Math.max(padding, rect.top),
      };
    }
  }

  return (
    <div className="fixed inset-0 z-[100] pointer-events-none">
      {/* Backdrop with cutout for the target */}
      {rect ? (
        <svg className="absolute inset-0 w-full h-full pointer-events-auto" onClick={finish}>
          <defs>
            <mask id="cc-tut-mask">
              <rect width="100%" height="100%" fill="white" />
              <rect
                x={rect.left - 4}
                y={rect.top - 4}
                width={rect.width + 8}
                height={rect.height + 8}
                rx={8}
                fill="black"
              />
            </mask>
          </defs>
          <rect width="100%" height="100%" fill="rgba(0,0,0,0.55)" mask="url(#cc-tut-mask)" />
          <rect
            x={rect.left - 4}
            y={rect.top - 4}
            width={rect.width + 8}
            height={rect.height + 8}
            rx={8}
            fill="none"
            stroke="rgb(59,130,246)"
            strokeWidth={2}
            pointerEvents="none"
          />
        </svg>
      ) : (
        <div
          className="absolute inset-0 bg-black/55 pointer-events-auto"
          onClick={finish}
        />
      )}

      {/* Popover card */}
      <div
        className="absolute pointer-events-auto bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-2xl p-4"
        style={{
          width: popWidth,
          ...(rect
            ? popStyle
            : {
                left: '50%',
                top: '50%',
                transform: 'translate(-50%, -50%)',
              }),
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3 mb-2">
          <h3 className="text-sm font-semibold text-zinc-900 dark:text-zinc-100">
            {step.title}
          </h3>
          <button
            type="button"
            onClick={finish}
            aria-label="Close tour"
            className="text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 -mt-1 -mr-1"
            title="Close"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor"
              strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M18 6 6 18M6 6l12 12" />
            </svg>
          </button>
        </div>
        <p className="text-xs leading-relaxed text-zinc-600 dark:text-zinc-400">
          {step.body}
        </p>
        <div className="mt-4 flex items-center justify-between gap-2">
          <span className="text-[10px] font-mono text-zinc-400 dark:text-zinc-500">
            {stepIndex + 1} / {STEPS.length}
          </span>
          <div className="flex items-center gap-1.5">
            <button
              type="button"
              onClick={finish}
              className="text-xs px-2 py-1 rounded text-zinc-500 dark:text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 transition-colors"
            >
              Skip
            </button>
            {!isFirst && (
              <button
                type="button"
                onClick={prev}
                className="text-xs px-3 py-1 rounded border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
              >
                Back
              </button>
            )}
            <button
              type="button"
              onClick={next}
              className="text-xs px-3 py-1 rounded bg-blue-600 hover:bg-blue-700 text-white font-medium transition-colors"
            >
              {isLast ? 'Done' : 'Next'}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
