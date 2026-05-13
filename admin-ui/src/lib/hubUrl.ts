// Public hub URL helpers — used by the install one-liner shown when a
// token is minted. Stored in localStorage so the admin only has to set
// it once. The auto-detected default falls back to the admin page's
// own scheme + hostname + the hub's standard 7100 port; that works
// when admin UI and hub WS share a host (the common case), and is at
// least a sensible starting point when they don't.

const KEY = 'cc_public_hub_url';

export function getStoredHubUrl(): string {
  return localStorage.getItem(KEY) ?? '';
}

export function setStoredHubUrl(v: string): void {
  const trimmed = v.trim();
  if (trimmed) localStorage.setItem(KEY, trimmed);
  else localStorage.removeItem(KEY);
}

export function guessHubUrl(): string {
  if (typeof window === 'undefined') return '';
  const proto = window.location.protocol === 'https:' ? 'https' : 'http';
  const host = window.location.hostname || 'localhost';
  return `${proto}://${host}:7100`;
}

export function resolveHubUrl(): string {
  return getStoredHubUrl() || guessHubUrl();
}
