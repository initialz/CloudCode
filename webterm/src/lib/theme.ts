// Theme management — mirrors admin-ui/src/lib/theme.ts exactly.
// Tailwind class-based dark mode: "dark" class on <html>.
//
// Three settings:
//   "system" — track prefers-color-scheme (default)
//   "light"  — force light
//   "dark"   — force dark

const KEY = 'cc_theme';

export type Theme = 'light' | 'dark' | 'system';

export function getStoredTheme(): Theme {
  const v = localStorage.getItem(KEY);
  return v === 'light' || v === 'dark' || v === 'system' ? v : 'system';
}

export function setStoredTheme(t: Theme): void {
  localStorage.setItem(KEY, t);
  apply(t);
}

/** Resolve "system" to the actual light/dark currently in effect. */
export function effectiveTheme(t: Theme): 'light' | 'dark' {
  if (t === 'system') {
    return window.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }
  return t;
}

/** Toggle .dark on <html>. */
export function apply(t: Theme): void {
  const effective = effectiveTheme(t);
  document.documentElement.classList.toggle('dark', effective === 'dark');
}

/**
 * Follow OS theme changes when user is on "system".
 * Returns a cleanup function. Call once at startup.
 */
export function watchSystem(): () => void {
  const mq = window.matchMedia?.('(prefers-color-scheme: dark)');
  if (!mq) return () => {};
  const onChange = () => {
    if (getStoredTheme() === 'system') apply('system');
  };
  mq.addEventListener('change', onChange);
  return () => mq.removeEventListener('change', onChange);
}
