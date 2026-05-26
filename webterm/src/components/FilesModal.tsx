// File-manager modal: browse and download files from an agent workspace.

import { useState, useEffect, useCallback, useRef } from 'react';
import { listFiles, downloadFileUrl, archiveUrl, uploadFiles, type FsEntry, type UploadResult } from '@/lib/api';

// ── Formatting helpers ───────────────────────────────────────────────────────

function formatBytes(n: number): string {
  if (n === 0) return '—';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

function formatRelative(ms: number): string {
  const diff = Date.now() - ms;
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return 'just now';
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const day = Math.floor(hr / 24);
  if (day === 1) return 'yesterday';
  if (day < 7) return `${day}d ago`;
  const wk = Math.floor(day / 7);
  if (wk < 5) return `${wk}w ago`;
  return new Date(ms).toLocaleDateString();
}

// ── Path helpers ─────────────────────────────────────────────────────────────

/** Navigate up one level from a path like "src/lib/" → "src/" or "" */
function parentPath(path: string): string {
  // Strip trailing slash, split, drop last segment, re-join
  const parts = path.replace(/\/$/, '').split('/').filter(Boolean);
  parts.pop();
  return parts.length === 0 ? '' : parts.join('/') + '/';
}

/** Breadcrumb segments from a path like "src/lib/" → [{label:"src",path:"src/"}, …] */
function breadcrumbs(path: string): { label: string; path: string }[] {
  const parts = path.replace(/\/$/, '').split('/').filter(Boolean);
  return parts.map((label, i) => ({
    label,
    path: parts.slice(0, i + 1).join('/') + '/',
  }));
}

// ── Icons ────────────────────────────────────────────────────────────────────

function FolderIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
    </svg>
  );
}

function FileIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
      <polyline points="14 2 14 8 20 8" />
    </svg>
  );
}

function LinkIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
      <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
    </svg>
  );
}

function DownloadIcon() {
  return (
    <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
      <polyline points="7 10 12 15 17 10" />
      <line x1="12" y1="15" x2="12" y2="3" />
    </svg>
  );
}

function UploadIcon() {
  return (
    <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
      <polyline points="17 8 12 3 7 8" />
      <line x1="12" y1="3" x2="12" y2="15" />
    </svg>
  );
}

function SpinnerIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"
      className="animate-spin text-zinc-400" aria-label="Loading">
      <path d="M12 2v4M12 18v4M4.93 4.93l2.83 2.83M16.24 16.24l2.83 2.83M2 12h4M18 12h4M4.93 19.07l2.83-2.83M16.24 7.76l2.83-2.83" />
    </svg>
  );
}

// ── VIRTUAL SCROLL ────────────────────────────────────────────────────────────

const ROW_HEIGHT = 28; // px, must match the `h-7` on each row
const VIRTUAL_THRESHOLD = 200;
const OVERSCAN = 5;

interface VirtualListProps {
  items: React.ReactNode[];
  totalCount: number;
}

function VirtualList({ items, totalCount }: VirtualListProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportHeight, setViewportHeight] = useState(400);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    setViewportHeight(el.clientHeight);
    const ro = new ResizeObserver(() => setViewportHeight(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const startIdx = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const visibleCount = Math.ceil(viewportHeight / ROW_HEIGHT) + OVERSCAN * 2;
  const endIdx = Math.min(totalCount - 1, startIdx + visibleCount);

  const paddingTop = startIdx * ROW_HEIGHT;
  const paddingBottom = (totalCount - 1 - endIdx) * ROW_HEIGHT;

  return (
    <div
      ref={containerRef}
      className="flex-1 overflow-y-auto"
      onScroll={(e) => setScrollTop((e.currentTarget).scrollTop)}
    >
      <div style={{ paddingTop, paddingBottom }}>
        {items.slice(startIdx, endIdx + 1)}
      </div>
    </div>
  );
}

// ── FilesModal ────────────────────────────────────────────────────────────────

type Props = {
  agent: string;
  workspace: string;
  onClose: () => void;
};

type LoadState =
  | { status: 'loading' }
  | { status: 'ok'; entries: FsEntry[]; serverErr: string | null }
  | { status: 'error'; message: string };

export default function FilesModal({ agent, workspace, onClose }: Props) {
  const [path, setPath] = useState('');
  const [showHidden, setShowHidden] = useState(true);
  const [load, setLoad] = useState<LoadState>({ status: 'loading' });
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const abortRef = useRef<AbortController | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // Upload state
  type UploadState = {
    files: File[];
    loaded: number;
    total: number;
    status: 'uploading' | 'done' | 'error';
    results?: UploadResult[];
    errorMsg?: string;
  };
  const [upload, setUpload] = useState<UploadState | null>(null);
  const [dragOver, setDragOver] = useState(false);

  // Conflict detection state
  type ConflictState = { all: File[]; conflicts: File[] };
  const [conflictFiles, setConflictFiles] = useState<ConflictState | null>(null);

  const selectionMode = selected.size > 0;

  const fetchList = useCallback(
    (targetPath: string, hidden: boolean) => {
      // Cancel any in-flight fetch
      if (abortRef.current) {
        abortRef.current.abort();
      }
      const ctrl = new AbortController();
      abortRef.current = ctrl;

      setLoad({ status: 'loading' });
      setSelected(new Set());

      listFiles(agent, workspace, targetPath, hidden, ctrl.signal)
        .then((res) => {
          setLoad({ status: 'ok', entries: res.entries, serverErr: res.error });
        })
        .catch((err: unknown) => {
          if (err instanceof Error && err.name === 'AbortError') return;
          const msg =
            err instanceof Error ? err.message : 'Unknown error';
          setLoad({ status: 'error', message: msg });
        });
    },
    [agent, workspace],
  );

  // Initial fetch
  useEffect(() => {
    fetchList(path, showHidden);
    return () => {
      if (abortRef.current) abortRef.current.abort();
    };
    // Only run on mount; navigations go through navigate()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Close on Escape
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  function navigate(nextPath: string) {
    setPath(nextPath);
    setSelected(new Set());
    fetchList(nextPath, showHidden);
  }

  function toggleHidden() {
    const next = !showHidden;
    setShowHidden(next);
    setSelected(new Set());
    fetchList(path, next);
  }

  function toggleSelection(name: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(name)) {
        next.delete(name);
      } else {
        next.add(name);
      }
      return next;
    });
  }

  function triggerDownload(filePath: string) {
    const url = downloadFileUrl(agent, workspace, filePath);
    // Use a hidden <a> to avoid navigating the SPA away
    const a = document.createElement('a');
    a.href = url;
    a.target = '_blank';
    a.rel = 'noreferrer';
    a.click();
  }

  function triggerArchiveDownload(paths: string[]) {
    const url = archiveUrl(agent, workspace, paths);
    const a = document.createElement('a');
    a.href = url;
    a.target = '_blank';
    a.rel = 'noreferrer';
    a.click();
  }

  function downloadSelected() {
    if (load.status !== 'ok' || selected.size === 0) return;
    const entries = load.entries;
    const fullPaths: string[] = [];
    for (const name of selected) {
      const entry = entries.find((e) => e.name === name);
      if (!entry) continue;
      const isDir = entry.kind === 'dir';
      fullPaths.push(path + name + (isDir ? '/' : ''));
    }
    triggerArchiveDownload(fullPaths);
  }

  function handleUpload(files: File[]) {
    if (load.status !== 'ok') return;

    const existingNames = new Set(load.entries.map(e => e.name));
    const conflicts = files.filter(f => existingNames.has(f.name));

    if (conflicts.length > 0) {
      setConflictFiles({ all: files, conflicts });
      return;
    }

    startUpload(files);
  }

  function startUpload(files: File[]) {
    const total = files.reduce((sum, f) => sum + f.size, 0);
    setUpload({ files, loaded: 0, total, status: 'uploading' });

    const { promise } = uploadFiles(agent, workspace, path, files, (loaded, t) => {
      setUpload(prev => prev ? { ...prev, loaded, total: t } : prev);
    });

    promise
      .then((results) => {
        setUpload(prev => prev ? { ...prev, status: 'done', results } : prev);
        // Refresh file list after short delay
        setTimeout(() => {
          setUpload(null);
          fetchList(path, showHidden);
        }, 1500);
      })
      .catch((err) => {
        setUpload(prev => prev ? { ...prev, status: 'error', errorMsg: err instanceof Error ? err.message : 'Upload failed' } : prev);
        setTimeout(() => setUpload(null), 3000);
      });
  }

  const crumbs = breadcrumbs(path);
  const title = `${workspace}@${agent}`;

  // Build row nodes
  function buildRows(): React.ReactNode[] {
    if (load.status !== 'ok') return [];
    const { entries } = load;
    const rows: React.ReactNode[] = [];

    // ".." row
    if (path !== '') {
      rows.push(
        <div
          key="__parent"
          className="h-7 flex items-center gap-2 px-3 cursor-pointer hover:bg-zinc-100 dark:hover:bg-zinc-800 text-xs font-mono text-zinc-500 dark:text-zinc-400"
          onClick={() => navigate(parentPath(path))}
        >
          <span className="w-3.5 shrink-0" />
          <span className="flex-1">../</span>
        </div>,
      );
    }

    for (const entry of entries) {
      const isDir = entry.kind === 'dir';
      const isSymlink = entry.kind === 'symlink';
      const isFile = entry.kind === 'file' || isSymlink || entry.kind === 'other';
      const isSelected = selected.has(entry.name);
      const entryPath = path + entry.name + (isDir ? '/' : '');

      rows.push(
        <div
          key={entry.name}
          className={`h-7 flex items-center gap-2 px-3 text-xs font-mono transition-colors cursor-pointer ${
            isSelected
              ? 'bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100'
              : 'hover:bg-zinc-100 dark:hover:bg-zinc-800 text-zinc-700 dark:text-zinc-300'
          }`}
          onClick={() => {
            if (isDir && !selectionMode) {
              // Navigate into directory when not in selection mode
              navigate(entryPath);
            } else {
              // Toggle selection for files always, and for dirs when in selection mode
              toggleSelection(entry.name);
            }
          }}
          onDoubleClick={() => {
            if (isFile) triggerDownload(entryPath);
          }}
        >
          {/* Icon */}
          <span className="w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500">
            {isDir ? <FolderIcon /> : isSymlink ? <LinkIcon /> : <FileIcon />}
          </span>

          {/* Name */}
          <span className="flex-1 truncate">
            {entry.name}{isDir ? '/' : ''}
          </span>

          {/* Size (files only) */}
          <span className="w-14 text-right shrink-0 text-zinc-400 dark:text-zinc-500">
            {isDir ? '' : formatBytes(entry.size)}
          </span>

          {/* mtime */}
          <span className="w-20 text-right shrink-0 text-zinc-400 dark:text-zinc-500">
            {formatRelative(entry.mtime_ms)}
          </span>

          {/* Download button */}
          <span className="w-6 shrink-0 flex justify-end">
            {isFile && (
              <button
                type="button"
                title={`Download ${entry.name}`}
                aria-label={`Download ${entry.name}`}
                className="p-0.5 rounded text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 hover:bg-zinc-200 dark:hover:bg-zinc-700 transition-colors"
                onClick={(e) => {
                  e.stopPropagation();
                  triggerDownload(entryPath);
                }}
              >
                <DownloadIcon />
              </button>
            )}
            {isDir && (
              <button
                type="button"
                title={`Download ${entry.name}/ as ZIP`}
                aria-label={`Download ${entry.name}/ as ZIP`}
                className="p-0.5 rounded text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 hover:bg-zinc-200 dark:hover:bg-zinc-700 transition-colors"
                onClick={(e) => {
                  e.stopPropagation();
                  triggerArchiveDownload([entryPath]);
                }}
              >
                <DownloadIcon />
              </button>
            )}
          </span>
        </div>,
      );
    }
    return rows;
  }

  const rows = buildRows();
  const entryCount =
    load.status === 'ok'
      ? load.entries.length
      : 0;
  const useVirtual = entryCount > VIRTUAL_THRESHOLD;
  const totalRows = rows.length;

  return (
    /* Backdrop */
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40"
      onClick={onClose}
    >
      {/* Modal panel */}
      <div
        className="flex flex-col bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-xl shadow-2xl w-full max-w-5xl mx-4 overflow-hidden"
        style={{ maxHeight: 'calc(100vh - 4rem)', minHeight: '36rem' }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-center gap-2 px-4 py-3 border-b border-zinc-200 dark:border-zinc-800 shrink-0">
          <span className="flex-1 text-sm font-semibold text-zinc-800 dark:text-zinc-200 font-mono truncate">
            Files — {title}
          </span>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="p-1 rounded text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 hover:bg-zinc-100 dark:hover:bg-zinc-800 transition-colors"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor"
              strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
              <path d="M18 6 6 18M6 6l12 12" />
            </svg>
          </button>
        </div>

        {/* Breadcrumb + toggle bar */}
        <div className="flex items-center gap-1 px-4 py-2 border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900/60 shrink-0 text-xs font-mono">
          {/* Breadcrumb */}
          <div className="flex items-center gap-0.5 flex-1 overflow-hidden">
            <button
              type="button"
              className="text-zinc-500 dark:text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100 shrink-0"
              onClick={() => navigate('')}
            >
              /
            </button>
            {crumbs.map((c) => (
              <span key={c.path} className="flex items-center gap-0.5 shrink-0">
                <span className="text-zinc-300 dark:text-zinc-700 select-none">›</span>
                <button
                  type="button"
                  className="text-zinc-500 dark:text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100"
                  onClick={() => navigate(c.path)}
                >
                  {c.label}
                </button>
              </span>
            ))}
          </div>

          {/* Show hidden toggle */}
          <label className="flex items-center gap-1.5 shrink-0 text-zinc-500 dark:text-zinc-400 cursor-pointer select-none">
            <input
              type="checkbox"
              checked={showHidden}
              onChange={toggleHidden}
              className="accent-zinc-600"
            />
            Show hidden
          </label>

          {/* Upload button */}
          <button
            type="button"
            onClick={() => fileInputRef.current?.click()}
            className="flex items-center gap-1 shrink-0 text-zinc-500 dark:text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100 transition-colors"
          >
            <UploadIcon />
            Upload
          </button>

          {/* Hidden file input */}
          <input
            ref={fileInputRef}
            type="file"
            multiple
            className="hidden"
            onChange={(e) => {
              if (e.target.files?.length) handleUpload(Array.from(e.target.files));
              e.target.value = '';
            }}
          />
        </div>

        {/* Column headers */}
        {load.status === 'ok' && load.entries.length > 0 && (
          <div className="flex items-center gap-2 px-3 py-1 border-b border-zinc-100 dark:border-zinc-800/60 bg-zinc-50 dark:bg-zinc-900/40 shrink-0 text-xs text-zinc-400 dark:text-zinc-600 font-mono">
            <span className="w-3.5 shrink-0" />
            <span className="flex-1">name</span>
            <span className="w-14 text-right shrink-0">size</span>
            <span className="w-20 text-right shrink-0">modified</span>
            <span className="w-6 shrink-0" />
          </div>
        )}

        {/* Body — drag & drop area */}
        <div
          className="relative flex-1 flex flex-col min-h-0"
          onDragOver={(e) => { e.preventDefault(); setDragOver(true); }}
          onDragLeave={() => setDragOver(false)}
          onDrop={(e) => {
            e.preventDefault();
            setDragOver(false);
            if (e.dataTransfer.files.length) handleUpload(Array.from(e.dataTransfer.files));
          }}
        >
          {/* Drag overlay */}
          {dragOver && (
            <div className="absolute inset-0 z-10 flex items-center justify-center bg-blue-50/80 dark:bg-blue-950/60 border-2 border-dashed border-blue-400 dark:border-blue-600 rounded-lg pointer-events-none">
              <span className="text-sm font-medium text-blue-600 dark:text-blue-400">Drop files to upload</span>
            </div>
          )}

          {/* Conflict dialog */}
          {conflictFiles && (
            <div className="absolute inset-0 z-20 flex items-center justify-center bg-black/30">
              <div className="bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-800 rounded-lg shadow-xl p-4 max-w-sm mx-4 text-sm">
                <p className="font-medium text-zinc-800 dark:text-zinc-200 mb-2">
                  {conflictFiles.conflicts.length} file{conflictFiles.conflicts.length > 1 ? 's' : ''} already exist{conflictFiles.conflicts.length === 1 ? 's' : ''}:
                </p>
                <ul className="text-xs text-zinc-600 dark:text-zinc-400 mb-3 max-h-32 overflow-y-auto font-mono">
                  {conflictFiles.conflicts.map(f => <li key={f.name}>{f.name}</li>)}
                </ul>
                <div className="flex gap-2 justify-end">
                  <button onClick={() => setConflictFiles(null)}
                    className="px-3 py-1.5 rounded border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-800">
                    Skip
                  </button>
                  <button onClick={() => {
                    const nonConflict = conflictFiles.all.filter(f => !conflictFiles.conflicts.some(c => c.name === f.name));
                    setConflictFiles(null);
                    if (nonConflict.length > 0) startUpload(nonConflict);
                  }}
                    className="px-3 py-1.5 rounded border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-800">
                    Skip conflicts
                  </button>
                  <button onClick={() => {
                    setConflictFiles(null);
                    startUpload(conflictFiles.all);
                  }}
                    className="px-3 py-1.5 rounded bg-blue-600 hover:bg-blue-700 text-white">
                    Overwrite
                  </button>
                </div>
              </div>
            </div>
          )}

          {load.status === 'loading' ? (
            <div className="flex-1 flex items-center justify-center py-12">
              <SpinnerIcon />
            </div>
          ) : load.status === 'error' ? (
            <div className="flex-1 flex flex-col items-center justify-center gap-3 py-12 px-4">
              <p className="text-sm text-red-600 dark:text-red-400 text-center">
                {load.message}
              </p>
              <button
                type="button"
                onClick={() => fetchList(path, showHidden)}
                className="text-xs px-3 py-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 text-zinc-600 dark:text-zinc-400 hover:bg-zinc-50 dark:hover:bg-zinc-800 transition-colors"
              >
                Retry
              </button>
            </div>
          ) : load.status === 'ok' && load.entries.length === 0 && path === '' ? (
            <div className="flex-1 flex items-center justify-center py-12 text-sm text-zinc-400 dark:text-zinc-600">
              {load.serverErr ?? 'Empty folder'}
            </div>
          ) : load.status === 'ok' && load.entries.length === 0 && path !== '' ? (
            <div className="flex-1 overflow-y-auto">
              {/* ".." row when dir is empty */}
              <div
                className="h-7 flex items-center gap-2 px-3 cursor-pointer hover:bg-zinc-100 dark:hover:bg-zinc-800 text-xs font-mono text-zinc-500 dark:text-zinc-400"
                onClick={() => navigate(parentPath(path))}
              >
                <span className="w-3.5 shrink-0" />
                <span className="flex-1">../</span>
              </div>
              <div className="flex items-center justify-center py-8 text-sm text-zinc-400 dark:text-zinc-600">
                {load.serverErr ?? 'Empty folder'}
              </div>
            </div>
          ) : useVirtual ? (
            <VirtualList items={rows} totalCount={totalRows} />
          ) : (
            <div className="flex-1 overflow-y-auto">
              {rows}
            </div>
          )}
        </div>

        {/* Server-side warning (non-fatal) */}
        {load.status === 'ok' && load.serverErr && load.entries.length > 0 && (
          <div className="px-4 py-1.5 border-t border-yellow-200 dark:border-yellow-900/60 bg-yellow-50 dark:bg-yellow-900/20 text-xs text-yellow-700 dark:text-yellow-400 shrink-0">
            {load.serverErr}
          </div>
        )}

        {/* Footer — item count + selection actions + upload progress */}
        <div className="shrink-0 h-9 flex items-center gap-2 px-4 border-t border-zinc-200 dark:border-zinc-800 text-xs font-mono">
          {upload ? (
            <>
              {upload.status === 'uploading' && (
                <>
                  <div className="flex-1 h-1.5 bg-zinc-200 dark:bg-zinc-700 rounded-full overflow-hidden">
                    <div className="h-full bg-blue-500 transition-all duration-200 rounded-full"
                      style={{ width: `${upload.total > 0 ? (upload.loaded / upload.total * 100) : 0}%` }} />
                  </div>
                  <span className="text-zinc-500 dark:text-zinc-400 shrink-0">
                    {upload.total > 0 ? `${Math.round(upload.loaded / upload.total * 100)}%` : 'Uploading...'}
                  </span>
                </>
              )}
              {upload.status === 'done' && (
                <span className="text-emerald-600 dark:text-emerald-400">Upload complete</span>
              )}
              {upload.status === 'error' && (
                <span className="text-red-600 dark:text-red-400">{upload.errorMsg}</span>
              )}
            </>
          ) : (
            <>
              <span className="text-zinc-400 dark:text-zinc-600">
                {load.status === 'ok'
                  ? `${load.entries.length} item${load.entries.length === 1 ? '' : 's'}`
                  : load.status === 'loading'
                  ? 'Loading...'
                  : 'Error'}
              </span>
              {selectionMode && (
                <>
                  <span className="text-zinc-300 dark:text-zinc-700">·</span>
                  <span className="text-blue-600 dark:text-blue-400">
                    {selected.size} selected
                  </span>
                  <button
                    type="button"
                    onClick={downloadSelected}
                    className="ml-auto px-2.5 py-0.5 rounded bg-blue-600 hover:bg-blue-700 text-white font-medium transition-colors"
                  >
                    Download ZIP
                  </button>
                  <button
                    type="button"
                    onClick={() => setSelected(new Set())}
                    className="px-2 py-0.5 rounded border border-zinc-200 dark:border-zinc-700 text-zinc-500 dark:text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-800 transition-colors"
                  >
                    Clear
                  </button>
                </>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
