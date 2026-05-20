//! Hub-side workspace storage.
//!
//! v1.13 moves the canonical copy of a workspace's files from "whatever
//! the agent has on disk" to "what the hub holds in
//! `<state>/hub/workspaces/<account>/<name>/`". Agents pull a working
//! copy at session start, push changes back, and have to take a hub-side
//! lock to do anything mutable. This module is the storage half of that
//! split — DB primitives for the lock + metadata live in `db.rs`, and
//! the wire protocol that ties the two together arrives in a later
//! phase.
//!
//! Everything here is sync stdlib I/O on purpose: workspace files are
//! local disk under our own state dir, the operations are not on a
//! request-path (sync engine runs them out-of-band), and the simplicity
//! is worth more than the marginal concurrency. If a future profile says
//! otherwise we can swap to `tokio::fs` without changing callers.

use anyhow::{anyhow, Context, Result};
use std::path::{Component, Path, PathBuf};

/// Filesystem-backed store for workspace contents. Every operation is
/// scoped under `<root>/<account>/<name>/`; callers cannot escape that
/// subtree even if they hand-craft `path` (see [`Self::validate_rel`]).
#[derive(Clone)]
pub struct WorkspaceStorage {
    root: PathBuf,
}

impl WorkspaceStorage {
    /// Open (or create) the workspace root. The directory tree is
    /// created lazily — only the root itself is mkdir'd here.
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating workspace root {}", root.display()))?;
        Ok(Self { root })
    }

    /// `<root>/<account>/<name>`. Does NOT create the directory; pair
    /// with [`Self::create_empty`] when you need it to exist.
    pub fn workspace_dir(&self, account: &str, name: &str) -> PathBuf {
        self.root.join(account).join(name)
    }

    /// Make sure `<root>/<account>/<name>/` exists. Idempotent.
    pub fn create_empty(&self, account: &str, name: &str) -> Result<()> {
        validate_segment(account, "account")?;
        validate_segment(name, "workspace name")?;
        let dir = self.workspace_dir(account, name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating workspace dir {}", dir.display()))?;
        Ok(())
    }

    /// Recursively delete the workspace directory. Missing dir is not
    /// an error — caller may be cleaning up a workspace that was never
    /// materialised on disk.
    pub fn delete(&self, account: &str, name: &str) -> Result<()> {
        validate_segment(account, "account")?;
        validate_segment(name, "workspace name")?;
        let dir = self.workspace_dir(account, name);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting {}", dir.display())),
        }
    }

    /// Write a file under `<root>/<account>/<name>/<path>`, creating
    /// parent directories as needed. Atomic against partial writes via
    /// tmp-file-then-rename within the same workspace directory.
    pub fn write_file(
        &self,
        account: &str,
        name: &str,
        path: &str,
        content: &[u8],
    ) -> Result<()> {
        let target = self.resolve_for_write(account, name, path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // Tmp file lives next to the target so the final rename is
        // guaranteed to be on the same filesystem (atomic rename).
        let tmp = tmp_sibling(&target);
        std::fs::write(&tmp, content)
            .with_context(|| format!("writing tmp {}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, &target) {
            // Best-effort cleanup of the orphan tmp file; ignore errors
            // (we'll surface the rename failure as the real one).
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| {
                format!("renaming {} -> {}", tmp.display(), target.display())
            });
        }
        Ok(())
    }

    /// Read the contents of a file inside the workspace. Errors if the
    /// file is missing or escapes the workspace root.
    pub fn read_file(&self, account: &str, name: &str, path: &str) -> Result<Vec<u8>> {
        let target = self.resolve_for_read(account, name, path)?;
        std::fs::read(&target).with_context(|| format!("reading {}", target.display()))
    }

    /// Remove a file or directory inside the workspace. Missing path
    /// is not an error (delete is idempotent). Directories get the
    /// recursive treatment because the agent's watcher emits one
    /// `Remove` event for a folder rather than per-child events when
    /// the whole tree is rm-rf'd at once.
    pub fn delete_file(&self, account: &str, name: &str, path: &str) -> Result<()> {
        let target = self.resolve_for_write(account, name, path)?;
        let meta = match std::fs::symlink_metadata(&target) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("stat-ing {} for delete", target.display()))
            }
        };
        let r = if meta.is_dir() {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                // Include the io error reason in the message so the
                // agent's ack log shows e.g. "Permission denied" /
                // "IsADirectory" rather than just the path.
                Err(anyhow::anyhow!(
                    "deleting {}: {}",
                    target.display(),
                    e
                ))
            }
        }
    }

    /// Walk the workspace, return `(relative path with `/` separators,
    /// size in bytes)` for every regular file, sorted by path. Missing
    /// workspace yields an empty list (callers commonly call this on a
    /// freshly-created workspace before any files have been pushed).
    pub fn list_files(&self, account: &str, name: &str) -> Result<Vec<(String, u64)>> {
        validate_segment(account, "account")?;
        validate_segment(name, "workspace name")?;
        let base = self.workspace_dir(account, name);
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        walk_files(&base, &base, &mut out)?;
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Sum of every regular file's byte size in the workspace. Uses the
    /// same walker as `list_files`; symlinks / non-regular entries are
    /// skipped, matching the on-disk semantics we want for the
    /// `workspaces.size_bytes` column.
    pub fn total_size(&self, account: &str, name: &str) -> Result<u64> {
        Ok(self
            .list_files(account, name)?
            .into_iter()
            .map(|(_, sz)| sz)
            .sum())
    }

    // -- helpers --------------------------------------------------------

    /// Resolve a workspace-relative path to an absolute path under the
    /// workspace root, after validating that it cannot escape. Used by
    /// every mutating op (write / delete_file).
    fn resolve_for_write(&self, account: &str, name: &str, path: &str) -> Result<PathBuf> {
        validate_segment(account, "account")?;
        validate_segment(name, "workspace name")?;
        let rel = Self::validate_rel(path)?;
        Ok(self.workspace_dir(account, name).join(rel))
    }

    /// Resolve a workspace-relative path for reads. Identical to the
    /// write variant today; kept separate so we can layer additional
    /// checks (e.g. existence, symlink-following policy) later without
    /// hunting every call site.
    fn resolve_for_read(&self, account: &str, name: &str, path: &str) -> Result<PathBuf> {
        self.resolve_for_write(account, name, path)
    }

    /// Reject any caller-supplied path that could escape the workspace
    /// directory. Returns a normalised `PathBuf` whose components are
    /// guaranteed to be plain `Normal` segments (no `.`, `..`, prefix,
    /// root) — safe to `.join()` onto the workspace dir.
    fn validate_rel(path: &str) -> Result<PathBuf> {
        if path.is_empty() {
            return Err(anyhow!("path is empty"));
        }
        if path.contains('\0') {
            return Err(anyhow!("path contains NUL byte"));
        }
        // Reject leading `/` and Windows-style absolute paths up front
        // so the error message is specific. The Component walk below
        // would catch them too, but with a less helpful message.
        if path.starts_with('/') || path.starts_with('\\') {
            return Err(anyhow!("path is absolute: {path}"));
        }
        let p = Path::new(path);
        let mut out = PathBuf::new();
        for comp in p.components() {
            match comp {
                Component::Normal(seg) => {
                    // Defence in depth — a segment from `Path::components`
                    // is already free of `/`, but check for NUL again
                    // since `Path::new` doesn't strip it.
                    let s = seg
                        .to_str()
                        .ok_or_else(|| anyhow!("non-utf8 path segment"))?;
                    if s.contains('\0') {
                        return Err(anyhow!("path segment contains NUL byte"));
                    }
                    out.push(s);
                }
                Component::CurDir => {
                    // `.` segments are harmless but indicate sloppy
                    // callers; reject so we don't end up with a path
                    // that visually doesn't match what's on disk.
                    return Err(anyhow!("path contains `.` segment: {path}"));
                }
                Component::ParentDir => {
                    return Err(anyhow!("path contains `..` segment: {path}"));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(anyhow!("path is absolute: {path}"));
                }
            }
        }
        if out.as_os_str().is_empty() {
            return Err(anyhow!("path resolves to empty: {path}"));
        }
        Ok(out)
    }
}

/// Validate an account or workspace name. These become directory names
/// directly, so the same anti-escape rules apply. Empty, `.`, `..`, or
/// any `/` / `\` / NUL is rejected.
fn validate_segment(seg: &str, what: &str) -> Result<()> {
    if seg.is_empty() {
        return Err(anyhow!("{what} is empty"));
    }
    if seg == "." || seg == ".." {
        return Err(anyhow!("{what} is reserved: {seg}"));
    }
    if seg.contains('/') || seg.contains('\\') || seg.contains('\0') {
        return Err(anyhow!("{what} contains path separator or NUL: {seg}"));
    }
    Ok(())
}

/// Construct a temp file path that sits next to `target` so the final
/// rename is on the same filesystem. Suffix is randomised so concurrent
/// writers to the same key don't clobber each other's tmp files.
fn tmp_sibling(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp.{}", uuid::Uuid::new_v4().simple()));
    match target.parent() {
        Some(p) => p.join(name),
        None => PathBuf::from(name),
    }
}

/// Depth-first walk emitting `(relative-path-with-forward-slashes, size)`
/// for every regular file. Symlinks and other non-regular entries are
/// skipped on purpose; the hub-canonical store is plain files only.
fn walk_files(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("iterating {}", dir.display()))?;
        // `metadata` (not `symlink_metadata`) so we follow into
        // directories but a broken/dangling symlink to a missing file
        // simply gets skipped (file_type().is_file() is false).
        let md = match entry.metadata() {
            Ok(md) => md,
            Err(_) => continue,
        };
        let ft = md.file_type();
        let path = entry.path();
        if ft.is_dir() {
            walk_files(base, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(base)
                .with_context(|| format!("strip_prefix {}", path.display()))?;
            // Forward-slash normalisation so the wire format is the
            // same regardless of host OS.
            let rel_str = rel
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel_str, md.len()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a fresh storage rooted inside the OS temp dir. Each test
    /// gets its own subdirectory so they can run in parallel without
    /// stepping on each other. We don't bother with a Drop guard — the
    /// OS cleans `/tmp` and the bytes per test are small.
    fn fresh() -> WorkspaceStorage {
        let root = std::env::temp_dir().join(format!(
            "cloudcode-ws-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        WorkspaceStorage::new(root).expect("storage init")
    }

    #[test]
    fn create_and_delete_round_trip() {
        let s = fresh();
        let dir = s.workspace_dir("alice", "demo");
        assert!(!dir.exists());
        s.create_empty("alice", "demo").unwrap();
        assert!(dir.is_dir());
        // delete is idempotent
        s.delete("alice", "demo").unwrap();
        assert!(!dir.exists());
        s.delete("alice", "demo").unwrap();
    }

    #[test]
    fn write_file_rejects_escapes() {
        let s = fresh();
        s.create_empty("alice", "demo").unwrap();
        let bad = [
            "../escape.txt",
            "a/../../etc/passwd",
            "/abs.txt",
            "/etc/passwd",
            "a/\0b",
            "",
            "./hidden.txt",
            ".",
            "..",
        ];
        for p in bad {
            let err = s.write_file("alice", "demo", p, b"x").unwrap_err();
            assert!(
                err.to_string().to_lowercase().contains("path")
                    || err.to_string().contains("empty"),
                "path {p:?} should be rejected, got: {err}"
            );
        }
        // backslash-leading is rejected as absolute too (defensive — on
        // unix it's just a weird filename, but the hub is multi-platform
        // in spirit).
        assert!(s.write_file("alice", "demo", "\\evil", b"x").is_err());
    }

    #[test]
    fn write_read_delete_nested_round_trip() {
        let s = fresh();
        s.create_empty("alice", "demo").unwrap();
        let payload = b"hello world".to_vec();
        s.write_file("alice", "demo", "src/lib/foo.rs", &payload)
            .unwrap();
        let got = s.read_file("alice", "demo", "src/lib/foo.rs").unwrap();
        assert_eq!(got, payload);
        // Overwrite works (atomic rename replaces in place).
        s.write_file("alice", "demo", "src/lib/foo.rs", b"v2")
            .unwrap();
        assert_eq!(
            s.read_file("alice", "demo", "src/lib/foo.rs").unwrap(),
            b"v2"
        );
        s.delete_file("alice", "demo", "src/lib/foo.rs").unwrap();
        assert!(s.read_file("alice", "demo", "src/lib/foo.rs").is_err());
        // delete_file is idempotent on missing files
        s.delete_file("alice", "demo", "src/lib/foo.rs").unwrap();
    }

    #[test]
    fn list_files_returns_sorted_relative_paths() {
        let s = fresh();
        s.create_empty("alice", "demo").unwrap();
        s.write_file("alice", "demo", "b.txt", b"bb").unwrap();
        s.write_file("alice", "demo", "a.txt", b"a").unwrap();
        s.write_file("alice", "demo", "nested/c.txt", b"ccc").unwrap();
        let files = s.list_files("alice", "demo").unwrap();
        assert_eq!(
            files,
            vec![
                ("a.txt".to_string(), 1),
                ("b.txt".to_string(), 2),
                ("nested/c.txt".to_string(), 3),
            ]
        );
    }

    #[test]
    fn list_files_on_missing_workspace_is_empty() {
        let s = fresh();
        assert!(s.list_files("alice", "ghost").unwrap().is_empty());
    }

    #[test]
    fn total_size_counts_only_regular_files() {
        let s = fresh();
        s.create_empty("alice", "demo").unwrap();
        s.write_file("alice", "demo", "a", b"1234").unwrap();
        s.write_file("alice", "demo", "sub/b", b"56789").unwrap();
        // An empty directory on its own shouldn't contribute to size.
        std::fs::create_dir_all(s.workspace_dir("alice", "demo").join("empty-subdir"))
            .unwrap();
        assert_eq!(s.total_size("alice", "demo").unwrap(), 4 + 5);
    }

    #[test]
    fn delete_file_handles_directory_target() {
        // Regression: agent's watcher emits a single `Remove(Folder)`
        // event when an entire subtree gets rm-rf'd; we send
        // WorkspaceDeleteFile for that path. The hub used to call
        // `remove_file` on it, which fails with `IsADirectory`, and
        // the agent retry-loops forever. delete_file must handle
        // both files and directories.
        let s = fresh();
        s.create_empty("alice", "demo").unwrap();
        // Lay out a nested dir with content.
        s.write_file("alice", "demo", "sub/dir/file.txt", b"x").unwrap();
        assert!(s.workspace_dir("alice", "demo").join("sub/dir/file.txt").exists());
        // Delete the parent directory by relative path.
        s.delete_file("alice", "demo", "sub/dir").unwrap();
        assert!(!s.workspace_dir("alice", "demo").join("sub/dir").exists());
        // Idempotent: deleting again is a no-op.
        s.delete_file("alice", "demo", "sub/dir").unwrap();
        // Deleting a regular file still works.
        s.write_file("alice", "demo", "lone.txt", b"y").unwrap();
        s.delete_file("alice", "demo", "lone.txt").unwrap();
        assert!(!s.workspace_dir("alice", "demo").join("lone.txt").exists());
    }

    #[test]
    fn account_and_name_segment_validation() {
        let s = fresh();
        assert!(s.create_empty("", "demo").is_err());
        assert!(s.create_empty("alice", "").is_err());
        assert!(s.create_empty("..", "demo").is_err());
        assert!(s.create_empty("alice", "..").is_err());
        assert!(s.create_empty("a/b", "demo").is_err());
        assert!(s.create_empty("alice", "a/b").is_err());
    }
}
