// Shared "upload-and-insert" pipeline for the terminal file-drop / paste-image
// feature. A dropped (or pasted) file lives on the user's machine, but `claude`
// runs on the remote agent — so "drop a file" means: upload it into the
// workspace's `.cloudcode/uploads/` folder, then inject an `@<path>` mention
// (claude's native "include this file" signal) into the PTY input stream, the
// same channel typed input uses.
//
// Used by both the drag-and-drop handler and the paste-image handler in
// Workbench.tsx.

import { uploadFiles, type UploadItem } from './api';
import type { Tab } from './tabs';

/** Fixed upload destination, per the design spec — never configurable. */
export const UPLOAD_DIR = '.cloudcode/uploads';

/** Callbacks the caller wires up so the pipeline can surface a lightweight
 *  progress indicator + error toast without owning any UI itself. */
export type UploadInsertHooks = {
  /** Fired as the upload streams; `loaded`/`total` are byte counts. Called
   *  with `done: true` once finished (success or failure) so the caller can
   *  clear its indicator. */
  onProgress?: (loaded: number, total: number) => void;
  onDone?: () => void;
  /** Surface an error to the user (e.g. a toast). */
  onError?: (message: string) => void;
};

/**
 * Upload `files` into the active tab's workspace and inject `@`-mentions for
 * every cleanly-uploaded file into the PTY.
 *
 * Reference string: for each result with `error == null`, build
 * `@.cloudcode/uploads/<result.name>` — using `result.name` (the FINAL name the
 * agent actually wrote, after any conflict ` (n)` suffix), NOT the local
 * filename. References are joined with single spaces with one trailing space,
 * then sent via `tab.ws.sendBinary(...)` (same channel as typed input).
 *
 * If the tab has no connected ws, nothing is sent (we never write to a closed
 * PTY). Upload failures for individual files are skipped; other files in the
 * same batch still get referenced.
 */
export async function uploadAndInsertFiles(
  tab: Tab,
  files: File[],
  hooks: UploadInsertHooks = {},
): Promise<void> {
  if (files.length === 0) return;

  // Never send to a closed PTY.
  if (!tab.ws.connected) {
    hooks.onError?.('Open a session first');
    return;
  }

  const items: UploadItem[] = files.map((file) => ({
    file,
    relativePath: file.name,
  }));

  try {
    const { promise } = uploadFiles(
      tab.agent,
      tab.workspace,
      UPLOAD_DIR,
      items,
      (loaded, total) => hooks.onProgress?.(loaded, total),
    );
    const results = await promise;

    // Only reference files that uploaded cleanly (error == null).
    const refs = results
      .filter((r) => r.error == null)
      .map((r) => `@${UPLOAD_DIR}/${r.name}`);

    // Surface per-file failures (other files still proceed).
    const failed = results.filter((r) => r.error != null);
    if (failed.length > 0) {
      const names = failed.map((r) => r.name).join(', ');
      hooks.onError?.(`Upload failed: ${names}`);
    }

    if (refs.length > 0) {
      // Space-joined references with one trailing space, sent on the same
      // binary channel as typed input. Re-check the ws is still connected —
      // the upload is async and the session may have dropped meanwhile.
      if (tab.ws.connected) {
        const str = refs.join(' ') + ' ';
        tab.ws.sendBinary(new TextEncoder().encode(str));
      } else {
        hooks.onError?.('Session closed before files could be inserted');
      }
    }
  } catch (err) {
    hooks.onError?.(err instanceof Error ? err.message : 'Upload failed');
  } finally {
    hooks.onDone?.();
  }
}

/**
 * Pull image files out of a paste event's clipboard items. Returns synthesised
 * `File`s named `pasted-<timestamp>.<ext>` (ext derived from the MIME subtype,
 * e.g. `image/png` → `png`). Empty if the clipboard holds no images — the
 * caller should then let xterm's normal paste proceed (don't preventDefault).
 */
export function imageFilesFromClipboard(items: DataTransferItemList): File[] {
  const out: File[] = [];
  for (let i = 0; i < items.length; i++) {
    const item = items[i];
    if (item.kind !== 'file' || !item.type.startsWith('image/')) continue;
    const blob = item.getAsFile();
    if (!blob) continue;
    const subtype = item.type.split('/')[1] || 'png';
    const ext = subtype.split(';')[0] || 'png';
    const name = `pasted-${Date.now()}.${ext}`;
    out.push(new File([blob], name, { type: item.type }));
  }
  return out;
}
