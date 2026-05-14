// Version helpers. The agent reports CARGO_PKG_VERSION ("1.6.4") in
// its Hello frame; GitHub Releases tags carry a leading "v" ("v1.6.4").
// Normalise on the frontend so the two are comparable.

// Older agent binaries either can't self-update (v1.6.0 / v1.6.1
// shipped a supervisor that ignored the agent/current symlink, so
// the new binary landed on disk but never started) or have other
// known wedge-y bugs that this codebase has since fixed. The admin
// UI hides anything below this tag from the update target dropdown
// so an operator can't roll forward into a broken release.
export const MIN_AGENT_TARGET_VERSION = 'v1.6.2';

export function normalizeVersion(v: string | null | undefined): string | null {
  if (v == null) return null;
  return v.startsWith('v') ? v.slice(1) : v;
}

export function versionsEqual(
  a: string | null | undefined,
  b: string | null | undefined,
): boolean {
  const na = normalizeVersion(a);
  const nb = normalizeVersion(b);
  return na !== null && nb !== null && na === nb;
}

/** Compare two semver-ish tags (with or without leading "v"). Returns -1 | 0 | 1. */
export function compareSemver(a: string, b: string): -1 | 0 | 1 {
  const parse = (s: string) =>
    normalizeVersion(s)!
      .split('.')
      .map((p) => Number.parseInt(p, 10) || 0);
  const [aMaj, aMin, aPat] = parse(a);
  const [bMaj, bMin, bPat] = parse(b);
  for (const [x, y] of [
    [aMaj, bMaj],
    [aMin, bMin],
    [aPat, bPat],
  ] as [number, number][]) {
    if (x > y) return 1;
    if (x < y) return -1;
  }
  return 0;
}
