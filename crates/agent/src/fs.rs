//! Filesystem ops for the workspace file-manager (proto v9).
//!
//! Two entry points:
//!   - [`list`] enumerates entries under a workspace-relative directory.
//!   - [`read_stream`] streams a workspace-relative file back to the hub
//!     as a series of base64-encoded `FsReadChunk` frames.
//!
//! Everything goes through [`resolve_safe`], which is the only thing
//! standing between an attacker-controlled `path` field and the agent's
//! host filesystem. The agent runs OUTSIDE the workspace sandbox (it's
//! the supervisor that applies sandbox-exec to spawned tmux/claude
//! children), so a traversal bug here would expose the entire user's
//! home directory.
//!
//! Security invariants enforced by `resolve_safe`:
//!   - the workspace base dir must already exist on disk (we never
//!     auto-create from a path request);
//!   - leading `/` is stripped from the request (otherwise `join`
//!     treats it as absolute and discards the base);
//!   - NUL bytes in the request are rejected outright;
//!   - the final canonicalized target must be a strict descendant of
//!     the canonicalized base, which catches `..` traversal AND
//!     symlinks pointing outside the workspace.

use crate::pty::OutFrame;
use crate::tunnel::{ClientMsg, FsEntry, FsKind};
use base64::Engine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

/// Hard cap on entries returned in a single `FsList`. Past this point
/// we truncate and append a synthetic "(truncated, N total)" sentinel
/// so the UI can warn instead of silently dropping rows.
const MAX_LIST_ENTRIES: usize = 10_000;

/// Max bytes per `FsReadChunk` frame. 64 KiB is small enough to keep
/// individual JSON frames manageable but large enough that we're not
/// paying per-frame overhead on multi-MB downloads.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Refuse to stream files larger than 1 GiB. The hub buffers each
/// response in memory (see `crates/hub/src/pty_session.rs` for the
/// pending-FsRead map), so unbounded sizes would let any account OOM
/// the hub.
const MAX_FILE_BYTES: u64 = 1 << 30; // 1 GiB

/// Resolve a workspace-relative path against `workspace_root/account/workspace`
/// and prove the result is still inside that base.
///
/// Returns the canonicalized absolute target on success, or a short
/// human-readable error string on failure. The error string is sent
/// back to the hub verbatim, so it must not leak filesystem layout —
/// stick to high-level reasons ("not found", "outside workspace", …).
pub fn resolve_safe(
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    requested: &str,
) -> Result<PathBuf, String> {
    if requested.as_bytes().contains(&0u8) {
        return Err("path contains NUL byte".into());
    }
    let base_raw = workspace_root.join(account).join(workspace);
    let base = std::fs::canonicalize(&base_raw)
        .map_err(|e| format!("workspace not found: {}", e))?;

    // Strip any number of leading slashes so the join doesn't discard
    // `base` (PathBuf::join on an absolute path replaces the receiver).
    let rel = requested.trim_start_matches('/');
    let target_raw = if rel.is_empty() {
        base.clone()
    } else {
        base.join(rel)
    };

    // canonicalize requires existence — that's what makes it a real
    // anti-symlink check rather than a syntactic prefix match.
    let target = std::fs::canonicalize(&target_raw)
        .map_err(|e| format!("path not found: {}", e))?;

    if !target.starts_with(&base) {
        return Err("path escapes workspace".into());
    }
    Ok(target)
}

/// Like [`resolve_safe`] but for writes: the target file does not need to
/// exist yet — only the *parent directory* must resolve inside the workspace.
/// If the parent doesn't exist it is created (mkdir -p). Returns the full
/// target path with the parent validated.
///
/// Security invariants are the same as `resolve_safe`:
///   - NUL bytes → reject
///   - leading `/` stripped
///   - canonicalized parent must be a strict descendant of the workspace base
pub fn resolve_safe_parent(
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    requested: &str,
) -> Result<PathBuf, String> {
    if requested.as_bytes().contains(&0u8) {
        return Err("path contains NUL byte".into());
    }
    let base_raw = workspace_root.join(account).join(workspace);
    let base = std::fs::canonicalize(&base_raw)
        .map_err(|e| format!("workspace not found: {}", e))?;

    let rel = requested.trim_start_matches('/');
    if rel.is_empty() {
        return Err("path is empty (cannot write to a directory)".into());
    }

    let target_raw = base.join(rel);

    // The file itself doesn't exist yet, but the parent directory must
    // resolve inside the workspace. If it doesn't exist, create it.
    let parent_raw = target_raw
        .parent()
        .ok_or_else(|| "path has no parent".to_string())?;

    if !parent_raw.exists() {
        std::fs::create_dir_all(parent_raw)
            .map_err(|e| format!("create parent dir: {}", e))?;
    }

    let parent = std::fs::canonicalize(parent_raw)
        .map_err(|e| format!("parent not found: {}", e))?;

    if !parent.starts_with(&base) {
        return Err("path escapes workspace".into());
    }

    // Reconstruct the target from the canonicalized parent + the
    // original filename. This keeps symlink-based escape via the
    // filename component impossible (the filename itself cannot be a
    // symlink since the file doesn't exist yet — and the parent has
    // been proven safe).
    let filename = target_raw
        .file_name()
        .ok_or_else(|| "path has no filename".to_string())?;
    Ok(parent.join(filename))
}

// ---------------------------------------------------------------------------
// Write sessions (stateful upload)
// ---------------------------------------------------------------------------

/// An open file-write session, created by `FsWriteInit` and appended to
/// by successive `FsWriteChunk` frames.
pub struct WriteSession {
    file: fs::File,
    bytes_written: u64,
}

/// Thread-safe map of in-flight write sessions, keyed by request_id.
pub type WriteSessions = Arc<Mutex<HashMap<Uuid, WriteSession>>>;

pub fn new_write_sessions() -> WriteSessions {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Hard cap on the total bytes we'll accept through a single write
/// session. Matches `MAX_FILE_BYTES` used for reads.
const MAX_WRITE_BYTES: u64 = 1 << 30; // 1 GiB

/// Called on `FsWriteInit`: validate the target path and create/truncate
/// the file. Stores a `WriteSession` in `sessions` so that subsequent
/// `FsWriteChunk` frames can append data.
pub async fn write_init(
    sessions: &WriteSessions,
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    path: &str,
    request_id: Uuid,
) -> Result<(), String> {
    let target = resolve_safe_parent(workspace_root, account, workspace, path)?;

    let file = fs::File::create(&target)
        .await
        .map_err(|e| format!("create file: {}", e))?;

    let mut map = sessions.lock().await;
    map.insert(request_id, WriteSession {
        file,
        bytes_written: 0,
    });
    Ok(())
}

/// Called on `FsWriteChunk`: decode the base64 payload and append it to
/// the open file. Returns `(bytes_written_total, is_eof)`.
///
/// - On non-eof chunks: writes data, returns running total + `false`.
/// - On eof: flushes, closes (removes from map), returns final total + `true`.
/// - On error: removes the session from the map and returns Err.
pub async fn write_chunk(
    sessions: &WriteSessions,
    request_id: Uuid,
    data_b64: &str,
    eof: bool,
) -> Result<(u64, bool), String> {
    let decoded = if data_b64.is_empty() {
        Vec::new()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| format!("base64 decode: {}", e))?
    };

    let mut map = sessions.lock().await;
    if !map.contains_key(&request_id) {
        return Err("no write session for this request_id".into());
    }

    // Write data if present.
    if !decoded.is_empty() {
        let session = map.get(&request_id).unwrap();
        let new_total = session.bytes_written + decoded.len() as u64;
        if new_total > MAX_WRITE_BYTES {
            map.remove(&request_id);
            return Err("upload exceeds 1 GiB limit".into());
        }
        // Need mutable access for write_all; re-borrow.
        let session = map.get_mut(&request_id).unwrap();
        if let Err(e) = session.file.write_all(&decoded).await {
            map.remove(&request_id);
            return Err(format!("write: {}", e));
        }
        session.bytes_written += decoded.len() as u64;
    }

    if eof {
        let sess = map.remove(&request_id).unwrap();
        let total = sess.bytes_written;
        sess.file
            .sync_all()
            .await
            .map_err(|e| format!("sync: {}", e))?;
        Ok((total, true))
    } else {
        let total = map.get(&request_id).unwrap().bytes_written;
        Ok((total, false))
    }
}

/// List entries directly under `rel_path` (relative to the workspace
/// root). Returns the entries sorted (dirs first, then files, each
/// alphabetical). Hidden entries (leading dot) are filtered out
/// unless `show_hidden` is true.
pub async fn list(
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    rel_path: &str,
    show_hidden: bool,
) -> Result<Vec<FsEntry>, String> {
    let target = resolve_safe(workspace_root, account, workspace, rel_path)?;

    // We resolved through symlinks, so `is_dir` is correct here — the
    // canonicalized path no longer contains any link components.
    let meta = fs::metadata(&target)
        .await
        .map_err(|e| format!("stat: {}", e))?;
    if !meta.is_dir() {
        return Err("not a directory".into());
    }

    let mut rd = fs::read_dir(&target)
        .await
        .map_err(|e| format!("read_dir: {}", e))?;

    let mut dirs: Vec<FsEntry> = Vec::new();
    let mut files: Vec<FsEntry> = Vec::new();
    let mut total: usize = 0;

    loop {
        match rd.next_entry().await {
            Ok(Some(entry)) => {
                let name = match entry.file_name().into_string() {
                    Ok(n) => n,
                    // Non-UTF8 filenames are skipped: the wire format
                    // is JSON strings, so we can't faithfully round-trip
                    // them anyway.
                    Err(_) => continue,
                };
                if !show_hidden && name.starts_with('.') {
                    continue;
                }
                total += 1;
                // Use symlink_metadata so symlinks are reported as
                // symlinks, not as whatever they happen to point at.
                let lmeta = match entry.metadata().await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let ft = lmeta.file_type();
                let kind = if ft.is_symlink() {
                    FsKind::Symlink
                } else if ft.is_dir() {
                    FsKind::Dir
                } else if ft.is_file() {
                    FsKind::File
                } else {
                    FsKind::Other
                };
                let size = if matches!(kind, FsKind::Dir) {
                    0
                } else {
                    lmeta.len()
                };
                let mtime_ms = lmeta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let row = FsEntry {
                    name,
                    kind,
                    size,
                    mtime_ms,
                };
                if matches!(row.kind, FsKind::Dir) {
                    dirs.push(row);
                } else {
                    files.push(row);
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("read_dir entry: {}", e)),
        }
    }

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = Vec::with_capacity(dirs.len() + files.len());
    out.extend(dirs);
    out.extend(files);

    if out.len() > MAX_LIST_ENTRIES {
        out.truncate(MAX_LIST_ENTRIES);
        out.push(FsEntry {
            name: format!("(truncated, {} total)", total),
            kind: FsKind::Other,
            size: 0,
            mtime_ms: 0,
        });
    }

    Ok(out)
}

/// Stream a workspace file back to the hub as `FsReadChunk` frames.
///
/// Always terminates the stream with a chunk where `eof = true` — on
/// success the last chunk carries any trailing bytes; on failure a
/// single chunk with `error = Some(...)` and empty `data_b64` is sent.
/// The hub uses `eof` to know when to flush its outgoing HTTP body, so
/// missing it would hang the client until the WS timeout.
pub async fn read_stream(
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    rel_path: &str,
    request_id: Uuid,
    tx: mpsc::Sender<OutFrame>,
) {
    if let Err(err) = read_stream_inner(workspace_root, account, workspace, rel_path, request_id, &tx).await {
        let _ = tx
            .send(OutFrame::Text(ClientMsg::FsReadChunk {
                request_id,
                data_b64: String::new(),
                eof: true,
                error: Some(err),
            }))
            .await;
    }
}

/// Inner read pipeline that returns `Err(String)` on any failure so
/// the wrapper can emit a single terminal error frame. On success it
/// has already pushed the final `eof = true` chunk and returns Ok.
async fn read_stream_inner(
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    rel_path: &str,
    request_id: Uuid,
    tx: &mpsc::Sender<OutFrame>,
) -> Result<(), String> {
    let target = resolve_safe(workspace_root, account, workspace, rel_path)?;

    let meta = fs::metadata(&target)
        .await
        .map_err(|e| format!("stat: {}", e))?;
    if meta.is_dir() {
        return Err("is a directory".into());
    }
    if !meta.is_file() {
        // Symlinks were resolved by canonicalize; whatever's left
        // (FIFOs, sockets, device files, …) we refuse rather than
        // potentially hanging on open().
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err("file too large".into());
    }

    let mut f = fs::File::open(&target)
        .await
        .map_err(|e| format!("open: {}", e))?;

    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    let mut peek: Option<Vec<u8>> = None;

    loop {
        let n = f
            .read(&mut buf)
            .await
            .map_err(|e| format!("read: {}", e))?;
        if n == 0 {
            // Flush whatever was buffered as the EOF chunk. If `peek`
            // is None the file was empty — send one terminal chunk
            // with empty data so the hub still observes the eof flag.
            let tail = peek.take().unwrap_or_default();
            let data_b64 = if tail.is_empty() {
                String::new()
            } else {
                base64::engine::general_purpose::STANDARD.encode(&tail)
            };
            tx.send(OutFrame::Text(ClientMsg::FsReadChunk {
                request_id,
                data_b64,
                eof: true,
                error: None,
            }))
            .await
            .map_err(|_| "tx closed".to_string())?;
            return Ok(());
        }

        // We can't mark `eof=true` until we've seen a 0-byte read,
        // because read() is allowed to return short. Buffer one
        // chunk ahead so the final chunk gets eof set correctly.
        if let Some(prev) = peek.take() {
            let data_b64 = base64::engine::general_purpose::STANDARD.encode(&prev);
            tx.send(OutFrame::Text(ClientMsg::FsReadChunk {
                request_id,
                data_b64,
                eof: false,
                error: None,
            }))
            .await
            .map_err(|_| "tx closed".to_string())?;
        }
        peek = Some(buf[..n].to_vec());
    }
}

/// Stream a zip archive of multiple workspace paths (files and/or
/// directories) back to the hub as `FsReadChunk` frames.
///
/// Always terminates with `eof = true` — either on the last data
/// chunk or in a single error frame. The hub uses the same
/// `fs_read_streams` routing as `FsRead`.
pub async fn archive_stream(
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    paths: &[String],
    request_id: Uuid,
    tx: mpsc::Sender<OutFrame>,
) {
    if let Err(err) = archive_stream_inner(workspace_root, account, workspace, paths, request_id, &tx).await {
        let _ = tx
            .send(OutFrame::Text(ClientMsg::FsReadChunk {
                request_id,
                data_b64: String::new(),
                eof: true,
                error: Some(err),
            }))
            .await;
    }
}

/// Inner archive pipeline — returns `Err(String)` so the wrapper can
/// emit a single terminal error frame.
async fn archive_stream_inner(
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    paths: &[String],
    request_id: Uuid,
    tx: &mpsc::Sender<OutFrame>,
) -> Result<(), String> {
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    if paths.is_empty() {
        return Err("no paths specified".into());
    }

    // Resolve the workspace base once so we can compute relative
    // archive entry names.
    let base_raw = workspace_root.join(account).join(workspace);
    let base = std::fs::canonicalize(&base_raw)
        .map_err(|e| format!("workspace not found: {}", e))?;

    // Build the zip in memory.
    let buf: Vec<u8> = Vec::new();
    let cursor = Cursor::new(buf);
    let mut archive = zip::ZipWriter::new(cursor);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for rel_path in paths {
        let target = resolve_safe(workspace_root, account, workspace, rel_path)?;
        let lmeta = std::fs::symlink_metadata(&target)
            .map_err(|e| format!("stat {}: {}", rel_path, e))?;

        if lmeta.is_file() {
            // Single file — archive entry is the workspace-relative path.
            let entry_name = target
                .strip_prefix(&base)
                .map_err(|_| "path escapes workspace".to_string())?
                .to_string_lossy()
                .to_string();
            let data = std::fs::read(&target)
                .map_err(|e| format!("read {}: {}", rel_path, e))?;
            archive
                .start_file(&entry_name, options)
                .map_err(|e| format!("zip start_file: {}", e))?;
            archive
                .write_all(&data)
                .map_err(|e| format!("zip write: {}", e))?;
        } else if lmeta.is_dir() {
            // Walk recursively, skip symlinks (don't follow).
            for entry in walkdir::WalkDir::new(&target)
                .follow_links(false)
                .into_iter()
            {
                let entry = entry.map_err(|e| format!("walkdir: {}", e))?;
                let emeta = entry.metadata().map_err(|e| format!("stat: {}", e))?;
                let entry_path = entry.path();
                let entry_name = entry_path
                    .strip_prefix(&base)
                    .map_err(|_| "walked path escapes workspace".to_string())?
                    .to_string_lossy()
                    .to_string();

                if entry_name.is_empty() {
                    // The workspace root itself — skip.
                    continue;
                }

                if emeta.is_dir() {
                    // Add directory entry.
                    archive
                        .add_directory(&entry_name, options)
                        .map_err(|e| format!("zip add_directory: {}", e))?;
                } else if emeta.is_file() {
                    let data = std::fs::read(entry_path)
                        .map_err(|e| format!("read {}: {}", entry_name, e))?;
                    archive
                        .start_file(&entry_name, options)
                        .map_err(|e| format!("zip start_file: {}", e))?;
                    archive
                        .write_all(&data)
                        .map_err(|e| format!("zip write: {}", e))?;
                } else if entry.path_is_symlink() {
                    // Store symlink as a regular file containing the
                    // link target text (security: never follow it).
                    let link_target = std::fs::read_link(entry_path)
                        .map_err(|e| format!("readlink {}: {}", entry_name, e))?;
                    let target_str = link_target.to_string_lossy();
                    archive
                        .start_file(&entry_name, options)
                        .map_err(|e| format!("zip start_file: {}", e))?;
                    archive
                        .write_all(target_str.as_bytes())
                        .map_err(|e| format!("zip write: {}", e))?;
                }
                // Other file types (FIFOs, sockets, …) are silently skipped.
            }
        } else {
            return Err(format!("unsupported file type: {}", rel_path));
        }
    }

    let cursor = archive
        .finish()
        .map_err(|e| format!("zip finish: {}", e))?;
    let zip_bytes = cursor.into_inner();

    if zip_bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("archive too large".into());
    }

    // Stream the zip bytes as base64-encoded FsReadChunk frames, using
    // the same one-chunk-ahead buffering pattern as read_stream_inner.
    let mut offset = 0usize;
    let mut peek: Option<&[u8]> = None;

    loop {
        let end = (offset + READ_CHUNK_BYTES).min(zip_bytes.len());
        let n = end - offset;
        if n == 0 {
            // No more data — flush whatever was buffered as the EOF chunk.
            let tail = peek.unwrap_or(&[]);
            let data_b64 = if tail.is_empty() {
                String::new()
            } else {
                base64::engine::general_purpose::STANDARD.encode(tail)
            };
            tx.send(OutFrame::Text(ClientMsg::FsReadChunk {
                request_id,
                data_b64,
                eof: true,
                error: None,
            }))
            .await
            .map_err(|_| "tx closed".to_string())?;
            return Ok(());
        }

        // Send the previously buffered chunk (not eof yet).
        if let Some(prev) = peek.take() {
            let data_b64 = base64::engine::general_purpose::STANDARD.encode(prev);
            tx.send(OutFrame::Text(ClientMsg::FsReadChunk {
                request_id,
                data_b64,
                eof: false,
                error: None,
            }))
            .await
            .map_err(|_| "tx closed".to_string())?;
        }
        peek = Some(&zip_bytes[offset..end]);
        offset = end;
    }
}

/// Delete one or more workspace-relative paths (files and/or directories).
///
/// Each path is validated via [`resolve_safe`] before deletion. Files are
/// removed with `tokio::fs::remove_file`; directories with
/// `tokio::fs::remove_dir_all`. All paths are attempted even if some fail;
/// the return value carries the list of successfully deleted paths and,
/// optionally, the first error encountered.
pub async fn delete(
    workspace_root: &Path,
    account: &str,
    workspace: &str,
    paths: &[String],
) -> (Vec<String>, Option<String>) {
    let mut deleted: Vec<String> = Vec::new();
    let mut first_error: Option<String> = None;

    for rel in paths {
        let target = match resolve_safe(workspace_root, account, workspace, rel) {
            Ok(t) => t,
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(format!("{}: {}", rel, e));
                }
                continue;
            }
        };

        let meta = match fs::symlink_metadata(&target).await {
            Ok(m) => m,
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(format!("{}: {}", rel, e));
                }
                continue;
            }
        };

        let result = if meta.is_dir() {
            fs::remove_dir_all(&target).await
        } else {
            fs::remove_file(&target).await
        };

        match result {
            Ok(()) => deleted.push(rel.clone()),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(format!("{}: {}", rel, e));
                }
            }
        }
    }

    (deleted, first_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    /// Build a workspace_root/account/workspace tree and return
    /// `(tmp, workspace_root_path, account, workspace, base_path)`.
    fn setup() -> (TempDir, PathBuf, String, String, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let account = "alice".to_string();
        let workspace = "demo".to_string();
        let base = tmp.path().join(&account).join(&workspace);
        stdfs::create_dir_all(&base).unwrap();
        let root = tmp.path().to_path_buf();
        (tmp, root, account, workspace, base)
    }

    #[test]
    fn resolves_empty_path_to_base() {
        let (_tmp, root, account, workspace, base) = setup();
        let got = resolve_safe(&root, &account, &workspace, "").unwrap();
        // Both sides are canonicalized; on macOS that includes the
        // /private prefix for /tmp.
        assert_eq!(got, stdfs::canonicalize(&base).unwrap());
    }

    #[test]
    fn strips_leading_slash() {
        let (_tmp, root, account, workspace, base) = setup();
        stdfs::create_dir(base.join("sub")).unwrap();
        let got = resolve_safe(&root, &account, &workspace, "/sub").unwrap();
        assert_eq!(got, stdfs::canonicalize(base.join("sub")).unwrap());
    }

    #[test]
    fn nested_legitimate_path() {
        let (_tmp, root, account, workspace, base) = setup();
        stdfs::create_dir_all(base.join("a/b/c")).unwrap();
        stdfs::write(base.join("a/b/c/file.txt"), b"hi").unwrap();
        let got = resolve_safe(&root, &account, &workspace, "a/b/c/file.txt").unwrap();
        assert!(got.starts_with(stdfs::canonicalize(&base).unwrap()));
    }

    #[test]
    fn rejects_dotdot_escape() {
        let (_tmp, root, account, workspace, _base) = setup();
        // Create a sibling file outside the workspace to make the
        // canonicalize succeed; we want the prefix check to fail,
        // not "not found".
        let outside = root.join(&account).join("evil.txt");
        stdfs::write(&outside, b"secret").unwrap();
        let err = resolve_safe(&root, &account, &workspace, "../evil.txt").unwrap_err();
        assert!(
            err.contains("escapes") || err.contains("not found"),
            "unexpected err: {}",
            err
        );
    }

    #[test]
    fn rejects_symlink_escape() {
        let (_tmp, root, account, workspace, base) = setup();
        // Make a target outside the workspace and symlink into it.
        let outside = root.join("etc_secrets.txt");
        stdfs::write(&outside, b"top secret").unwrap();
        symlink(&outside, base.join("passwd_link")).unwrap();
        let err = resolve_safe(&root, &account, &workspace, "passwd_link").unwrap_err();
        assert!(
            err.contains("escapes"),
            "expected escape error, got: {}",
            err
        );
    }

    #[test]
    fn rejects_nonexistent_path() {
        let (_tmp, root, account, workspace, _base) = setup();
        let err = resolve_safe(&root, &account, &workspace, "missing.txt").unwrap_err();
        assert!(err.contains("not found"), "got: {}", err);
    }

    #[test]
    fn rejects_nul_byte() {
        let (_tmp, root, account, workspace, _base) = setup();
        let err = resolve_safe(&root, &account, &workspace, "ok\0bad").unwrap_err();
        assert!(err.contains("NUL"), "got: {}", err);
    }

    #[test]
    fn rejects_missing_workspace_base() {
        let (_tmp, root, _account, _workspace, _base) = setup();
        // base for a different (account, workspace) was never created.
        let err = resolve_safe(&root, "ghost", "ghost", "").unwrap_err();
        assert!(
            err.contains("workspace not found"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn list_sorts_dirs_first_then_files() {
        let (_tmp, root, account, workspace, base) = setup();
        stdfs::create_dir(base.join("zdir")).unwrap();
        stdfs::create_dir(base.join("adir")).unwrap();
        stdfs::write(base.join("zfile"), b"").unwrap();
        stdfs::write(base.join("afile"), b"hello").unwrap();
        let entries = list(&root, &account, &workspace, "", false).await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["adir", "zdir", "afile", "zfile"]);
        // Sanity: kinds reflect what we created.
        assert_eq!(entries[0].kind, FsKind::Dir);
        assert_eq!(entries[2].kind, FsKind::File);
        // Size populated for files.
        let afile = &entries[2];
        assert_eq!(afile.size, 5);
    }

    #[tokio::test]
    async fn list_hides_dotfiles_by_default() {
        let (_tmp, root, account, workspace, base) = setup();
        stdfs::write(base.join(".secret"), b"").unwrap();
        stdfs::write(base.join("visible"), b"").unwrap();
        let entries = list(&root, &account, &workspace, "", false).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "visible");
        let with_hidden = list(&root, &account, &workspace, "", true).await.unwrap();
        assert_eq!(with_hidden.len(), 2);
    }

    #[tokio::test]
    async fn list_rejects_non_dir() {
        let (_tmp, root, account, workspace, base) = setup();
        stdfs::write(base.join("file"), b"").unwrap();
        let err = list(&root, &account, &workspace, "file", false)
            .await
            .unwrap_err();
        assert!(err.contains("not a directory"), "got: {}", err);
    }

    // --- resolve_safe_parent tests ---

    #[test]
    fn resolve_safe_parent_creates_missing_parent() {
        let (_tmp, root, account, workspace, base) = setup();
        let got = resolve_safe_parent(&root, &account, &workspace, "new_dir/file.txt").unwrap();
        assert!(got.ends_with("file.txt"));
        // The parent must have been created.
        assert!(base.join("new_dir").exists());
    }

    #[test]
    fn resolve_safe_parent_rejects_dotdot_escape() {
        let (_tmp, root, account, workspace, _base) = setup();
        // Need a sibling dir for canonicalize to succeed on the parent.
        let outside = root.join(&account);
        stdfs::create_dir_all(&outside).unwrap();
        let err = resolve_safe_parent(&root, &account, &workspace, "../evil.txt").unwrap_err();
        assert!(
            err.contains("escapes") || err.contains("not found"),
            "got: {}",
            err
        );
    }

    #[test]
    fn resolve_safe_parent_rejects_nul_byte() {
        let (_tmp, root, account, workspace, _base) = setup();
        let err = resolve_safe_parent(&root, &account, &workspace, "ok\0bad").unwrap_err();
        assert!(err.contains("NUL"), "got: {}", err);
    }

    #[test]
    fn resolve_safe_parent_rejects_empty_path() {
        let (_tmp, root, account, workspace, _base) = setup();
        let err = resolve_safe_parent(&root, &account, &workspace, "").unwrap_err();
        assert!(err.contains("empty"), "got: {}", err);
    }

    #[test]
    fn resolve_safe_parent_strips_leading_slash() {
        let (_tmp, root, account, workspace, base) = setup();
        let got = resolve_safe_parent(&root, &account, &workspace, "/newfile.txt").unwrap();
        let canon_base = stdfs::canonicalize(&base).unwrap();
        assert_eq!(got, canon_base.join("newfile.txt"));
    }

    // --- write_init / write_chunk tests ---

    #[tokio::test]
    async fn write_init_creates_file() {
        let (_tmp, root, account, workspace, base) = setup();
        let sessions = new_write_sessions();
        let rid = Uuid::new_v4();
        write_init(&sessions, &root, &account, &workspace, "hello.txt", rid)
            .await
            .unwrap();
        // File should exist (empty, truncated).
        let canon_base = stdfs::canonicalize(&base).unwrap();
        assert!(canon_base.join("hello.txt").exists());
        // Session should be tracked.
        assert!(sessions.lock().await.contains_key(&rid));
    }

    #[tokio::test]
    async fn write_full_round_trip() {
        let (_tmp, root, account, workspace, base) = setup();
        let sessions = new_write_sessions();
        let rid = Uuid::new_v4();
        write_init(&sessions, &root, &account, &workspace, "sub/data.bin", rid)
            .await
            .unwrap();

        let payload = b"Hello, world!";
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);

        // Non-eof chunk.
        let (written, is_eof) = write_chunk(&sessions, rid, &b64, false).await.unwrap();
        assert!(!is_eof);
        assert_eq!(written, payload.len() as u64);

        // EOF chunk (empty data).
        let (total, is_eof) = write_chunk(&sessions, rid, "", true).await.unwrap();
        assert!(is_eof);
        assert_eq!(total, payload.len() as u64);

        // Session should be removed.
        assert!(!sessions.lock().await.contains_key(&rid));

        // File should contain the data.
        let canon_base = stdfs::canonicalize(&base).unwrap();
        let content = stdfs::read(canon_base.join("sub").join("data.bin")).unwrap();
        assert_eq!(content, payload);
    }

    #[tokio::test]
    async fn write_chunk_rejects_unknown_request_id() {
        let sessions = new_write_sessions();
        let err = write_chunk(&sessions, Uuid::new_v4(), "", true)
            .await
            .unwrap_err();
        assert!(err.contains("no write session"), "got: {}", err);
    }

    // --- delete tests ---

    #[tokio::test]
    async fn delete_removes_file() {
        let (_tmp, root, account, workspace, base) = setup();
        stdfs::write(base.join("doomed.txt"), b"bye").unwrap();
        let (deleted, error) = delete(&root, &account, &workspace, &["doomed.txt".into()]).await;
        assert_eq!(deleted, vec!["doomed.txt"]);
        assert!(error.is_none(), "unexpected error: {:?}", error);
        assert!(!base.join("doomed.txt").exists());
    }

    #[tokio::test]
    async fn delete_removes_directory_recursively() {
        let (_tmp, root, account, workspace, base) = setup();
        stdfs::create_dir_all(base.join("mydir/sub")).unwrap();
        stdfs::write(base.join("mydir/sub/file.txt"), b"x").unwrap();
        let (deleted, error) = delete(&root, &account, &workspace, &["mydir".into()]).await;
        assert_eq!(deleted, vec!["mydir"]);
        assert!(error.is_none(), "unexpected error: {:?}", error);
        assert!(!base.join("mydir").exists());
    }

    #[tokio::test]
    async fn delete_partial_success() {
        let (_tmp, root, account, workspace, base) = setup();
        stdfs::write(base.join("good.txt"), b"ok").unwrap();
        // "missing.txt" does not exist — resolve_safe will fail.
        let (deleted, error) = delete(
            &root,
            &account,
            &workspace,
            &["good.txt".into(), "missing.txt".into()],
        )
        .await;
        assert_eq!(deleted, vec!["good.txt"]);
        assert!(error.is_some());
        assert!(!base.join("good.txt").exists());
    }

    #[tokio::test]
    async fn delete_rejects_path_escape() {
        let (_tmp, root, account, workspace, _base) = setup();
        // Create a sibling file outside the workspace.
        let outside = root.join(&account).join("evil.txt");
        stdfs::write(&outside, b"secret").unwrap();
        let (deleted, error) = delete(
            &root,
            &account,
            &workspace,
            &["../evil.txt".into()],
        )
        .await;
        assert!(deleted.is_empty());
        assert!(error.is_some());
        // The file must still exist.
        assert!(outside.exists());
    }
}
