// Theme management. Tailwind is now configured for class-based dark
// mode (see tailwind.config.js). The class lives on <html>: present
// => dark, absent => light.
//
// Three settings:
//   - "system" — track prefers-color-scheme (default for fresh installs)
//   - "light"  — force light
//   - "dark"   — force dark
//
// `apply` is exported so main.tsx can run it BEFORE React mounts;
// otherwise the first paint would flash the wrong palette.

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
    return window.matchMedia?.('(prefers-color-scheme: dark)').matches
      ? 'dark'
      : 'light';
  }
  return t;
}

/** Toggle the .dark class on <html> according to the given theme. */
export function apply(t: Theme): void {
  const effective = effectiveTheme(t);
  document.documentElement.classList.toggle('dark', effective === 'dark');
}

/**
 * If the user is on "system" mode, follow OS theme changes live.
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
