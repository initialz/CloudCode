//! Gitignore-style filter for the workspace sync engine.
//!
//! Two layers stacked in order (later overrides earlier):
//!
//! 1. Hardcoded defaults — package manager output, VCS metadata,
//!    Python virtualenvs, build dirs, log files, OS junk.
//! 2. An optional `.cloudcodeignore` at the workspace root, parsed
//!    with gitignore syntax. Negations (`!pattern`) work, so an
//!    operator can override a default ignore for a specific path.
//!
//! Path inputs are always **workspace-relative** — the caller has
//! already stripped the workspace root. This keeps the matcher
//! independent of where the workspace lives on disk and makes it
//! cheap to unit-test.

use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

/// Hardcoded default ignore patterns. Kept here (rather than written
/// to `.cloudcodeignore` at workspace creation) so an operator can't
/// accidentally delete them.
///
/// Order doesn't matter — gitignore semantics are "last matching
/// pattern wins", and these never contradict each other.
pub const DEFAULT_IGNORES: &[&str] = &[
    "node_modules/",
    "target/",
    // NOTE: `.git/` is intentionally NOT in this list. We sync it so
    // the hub-canonical copy has the full git history (branches,
    // stashes, recent commits) the user expects when they open the
    // workspace from a different agent. Users who don't want it
    // synced can add `.git/` to `.cloudcodeignore`.
    "dist/",
    "build/",
    ".venv/",
    "venv/",
    "__pycache__/",
    ".next/",
    "*.log",
    ".DS_Store",
    ".cache/",
];

/// Filter that decides whether a workspace-relative path should be
/// excluded from sync.
#[derive(Debug)]
pub struct IgnoreFilter {
    matcher: Gitignore,
}

impl IgnoreFilter {
    /// Build a filter for the given workspace root.
    ///
    /// `workspace_root` is only used as the anchor passed to
    /// `GitignoreBuilder::new` (gitignore matching is rooted), and as
    /// the place to look for `.cloudcodeignore`. The root does not
    /// have to exist on disk — useful for tests.
    pub fn new(workspace_root: &Path) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(workspace_root);
        for pat in DEFAULT_IGNORES {
            builder
                .add_line(None, pat)
                .map_err(|e| anyhow::anyhow!("default ignore {pat:?}: {e}"))?;
        }
        // Layer the user's overrides on top. `add()` reads the file
        // from disk and returns `Some(err)` on parse failure (not on
        // missing file — that just returns `None`).
        let cci = workspace_root.join(".cloudcodeignore");
        if cci.exists() {
            if let Some(err) = builder.add(&cci) {
                return Err(anyhow::anyhow!(
                    ".cloudcodeignore at {}: {err}",
                    cci.display()
                ));
            }
        }
        let matcher = builder
            .build()
            .map_err(|e| anyhow::anyhow!("build ignore matcher: {e}"))?;
        Ok(Self { matcher })
    }

    /// True if `relative_path` should be skipped by the sync engine.
    ///
    /// `is_dir` matters because gitignore's `node_modules/` (with
    /// trailing slash) only matches directories. notify events tell
    /// us when something is a dir; on `stat` failure (file already
    /// deleted by the time we check) callers should pass `false`,
    /// which is the safer default — files inside ignored dirs are
    /// still caught via `matched_path_or_any_parents`.
    pub fn is_ignored(&self, relative_path: &Path, is_dir: bool) -> bool {
        // `matched_path_or_any_parents` walks up the path and matches
        // each ancestor too, so `node_modules/foo/bar.js` is caught
        // by the `node_modules/` rule even though the rule wouldn't
        // match `node_modules/foo/bar.js` directly.
        let m = self
            .matcher
            .matched_path_or_any_parents(relative_path, is_dir);
        m.is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn rel(p: &str) -> PathBuf {
        PathBuf::from(p)
    }

    #[test]
    fn matches_node_modules_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let f = IgnoreFilter::new(tmp.path()).unwrap();
        assert!(f.is_ignored(&rel("node_modules"), true));
        assert!(f.is_ignored(&rel("node_modules/foo"), false));
        assert!(f.is_ignored(&rel("node_modules/foo/bar.js"), false));
        assert!(f.is_ignored(&rel("packages/x/node_modules/dep/index.js"), false));
    }

    #[test]
    fn matches_target_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let f = IgnoreFilter::new(tmp.path()).unwrap();
        assert!(f.is_ignored(&rel("target/x.rlib"), false));
        assert!(f.is_ignored(&rel("target/debug/build/foo/out"), false));
    }

    #[test]
    fn does_not_match_source_files() {
        let tmp = tempfile::tempdir().unwrap();
        let f = IgnoreFilter::new(tmp.path()).unwrap();
        assert!(!f.is_ignored(&rel("src/main.rs"), false));
        assert!(!f.is_ignored(&rel("Cargo.toml"), false));
        assert!(!f.is_ignored(&rel("docs/readme.md"), false));
    }

    #[test]
    fn matches_glob_log() {
        let tmp = tempfile::tempdir().unwrap();
        let f = IgnoreFilter::new(tmp.path()).unwrap();
        assert!(f.is_ignored(&rel("server.log"), false));
        assert!(f.is_ignored(&rel("logs/app.log"), false));
        // `*.log` is a filename glob, not a directory match.
        assert!(!f.is_ignored(&rel("logs/app.txt"), false));
    }

    #[test]
    fn matches_ds_store_and_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let f = IgnoreFilter::new(tmp.path()).unwrap();
        assert!(f.is_ignored(&rel(".DS_Store"), false));
        assert!(f.is_ignored(&rel("subdir/.DS_Store"), false));
        assert!(f.is_ignored(&rel(".cache"), true));
        assert!(f.is_ignored(&rel(".cache/whatever"), false));
    }

    #[test]
    fn cloudcodeignore_adds_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".cloudcodeignore"), "secrets/\n*.bak\n").unwrap();
        let f = IgnoreFilter::new(tmp.path()).unwrap();
        assert!(f.is_ignored(&rel("secrets/api.key"), false));
        assert!(f.is_ignored(&rel("old.bak"), false));
        // Defaults still active.
        assert!(f.is_ignored(&rel("node_modules/x"), false));
        // Unrelated path still allowed.
        assert!(!f.is_ignored(&rel("src/lib.rs"), false));
    }

    #[test]
    fn cloudcodeignore_can_negate_default() {
        let tmp = tempfile::tempdir().unwrap();
        // Negate `*.log` so log files are tracked again.
        fs::write(tmp.path().join(".cloudcodeignore"), "!*.log\n").unwrap();
        let f = IgnoreFilter::new(tmp.path()).unwrap();
        assert!(!f.is_ignored(&rel("server.log"), false));
        // Other defaults still apply.
        assert!(f.is_ignored(&rel("node_modules/x"), false));
    }

    #[test]
    fn missing_cloudcodeignore_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        // No file written — must not error.
        let f = IgnoreFilter::new(tmp.path()).unwrap();
        assert!(!f.is_ignored(&rel("src/main.rs"), false));
    }
}
