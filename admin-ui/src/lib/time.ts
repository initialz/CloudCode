// All timestamps in the admin UI come from the hub as Unix epoch
// seconds (UTC). Render them in the operator's local timezone — most
// admin pages are read by someone reasoning about "what happened in
// the last hour", which is local-relative.
//
// Format is ISO-ish (sv-SE locale): "YYYY-MM-DD HH:MM:SS" / "YYYY-MM-DD".
// Sortable, monospace-friendly, locale-independent appearance, and no
// AM/PM ambiguity in dense tables.

/** "2026-05-14 21:34:42" in the browser's local timezone. */
export function formatDateTime(unix: number): string {
  return new Date(unix * 1000).toLocaleString('sv-SE', {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  });
}

/** "2026-05-14" in local timezone. */
export function formatDate(unix: number): string {
  return new Date(unix * 1000).toLocaleString('sv-SE', {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
  });
}

/** "21" — local hour for chart tick labels. */
export function formatHour2(unix: number): string {
  return new Date(unix * 1000).toLocaleString('sv-SE', {
    hour: '2-digit',
    hour12: false,
  });
}

/** "2026-05-14 21" for hour-bucket tooltips. */
export function formatDateHour(unix: number): string {
  // sv-SE doesn't pull in seconds/minutes when they're not requested
  // — but it does emit a trailing " 00:00" for hour-only formats on
  // some engines. Compose from date + hour instead for stability.
  return `${formatDate(unix)} ${formatHour2(unix)}`;
}
