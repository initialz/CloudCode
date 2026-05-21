//! Agent-level audit pipeline: capture every prompt the user types into
//! a `claude` session running under this agent, and ship it to the hub
//! for centralised storage.
//!
//! Why not piggy-back on `crate::jsonl`?
//! - `jsonl.rs` runs **per cloudcode-session**, lives in `PtyManager`,
//!   and forwards every line as `ClientMsg::Message` (raw body, no
//!   filtering). It's tied to a specific session's cwd / project dir,
//!   stops when the PTY dies, and explicitly does not filter to user
//!   input only.
//! - This module runs **process-wide**, watches the full
//!   `~/.claude/projects/` tree, filters to actual user inputs
//!   (prompt / bash-input), and survives PTY churn. The two pipelines
//!   write into different hub tables (`messages` vs
//!   `user_interactions`) by design — one is for the conversation view,
//!   the other for compliance/audit/search.
//!
//! Lifetime:
//! - Spawned once from `serve()` after PtyManager construction.
//! - Watcher (`notify::RecommendedWatcher`) is recursive on
//!   `~/.claude/projects/`. macOS FSEvents is flaky for newly-created
//!   subdirs, so we also wake on a 2s tick and rescan known files.
//! - The active hub `mpsc::Sender<OutFrame>` is exposed via
//!   `SenderSlot`. `ws::run_once` calls `set` on connect and clears it
//!   on disconnect. While the slot is empty we **do not advance
//!   offsets** — accumulated lines flush on next reconnect. The agent
//!   process owning the offset map is the source of truth across the
//!   life of the agent; restart re-tails from offset 0 and relies on
//!   the hub's UNIQUE constraint to dedupe (see db.rs).
//!
//! Errors (IO / parse / send) are warn-logged only — audit must never
//! crash the agent or interfere with PTY traffic.

use crate::pty::OutFrame;
use crate::tunnel::ClientMsg;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

/// Shared slot the WS layer plugs the current hub sender into. Cleared
/// on disconnect so the audit task knows to pause until reconnect.
#[derive(Clone, Default)]
pub struct SenderSlot {
    inner: Arc<Mutex<Option<mpsc::Sender<OutFrame>>>>,
}

impl SenderSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, tx: mpsc::Sender<OutFrame>) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(tx);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            *g = None;
        }
    }

    fn current(&self) -> Option<mpsc::Sender<OutFrame>> {
        self.inner.lock().ok().and_then(|g| g.clone())
    }
}

/// Spawn the long-running audit task. Returns immediately. The task
/// runs for the lifetime of the agent process; there is no shutdown
/// handle because the agent only stops on process exit anyway.
///
/// `workspace_root` is the absolute path the agent stores
/// per-account/workspace dirs under (e.g.
/// `/Users/x/cloudcode-agent/workspaces`). Sessions whose `.cwd`
/// doesn't sit under this root are ignored — those are the user's own
/// local claude runs that happen to share `~/.claude/projects/`.
pub fn spawn(home: PathBuf, workspace_root: PathBuf, slot: SenderSlot) {
    // Canonicalize so `cwd_path.starts_with(workspace_root)` actually
    // matches: agent.toml may have a relative path like
    // `./agent/workspaces` while claude writes the cwd to its jsonl
    // as an absolute `/Users/.../agent/workspaces/...`. Without this
    // step every interaction silently fails the prefix gate. If
    // canonicalize fails (dir doesn't exist yet, broken symlink,
    // …), fall back to the raw path so spawn still proceeds — the
    // gate will just keep rejecting until the dir appears, at which
    // point the agent will need a restart to pick it up.
    let workspace_root = std::fs::canonicalize(&workspace_root).unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            path = %workspace_root.display(),
            "audit: workspace_root canonicalize failed; using raw path"
        );
        workspace_root
    });
    tracing::info!(
        workspace_root = %workspace_root.display(),
        "audit: watcher starting"
    );
    tokio::spawn(async move {
        if let Err(e) = run(home, workspace_root, slot).await {
            tracing::warn!(error = %e, "audit: task exited");
        }
    });
}

async fn run(
    home: PathBuf,
    workspace_root: PathBuf,
    slot: SenderSlot,
) -> anyhow::Result<()> {
    let projects_dir = home.join(".claude").join("projects");
    if let Err(e) = tokio::fs::create_dir_all(&projects_dir).await {
        tracing::warn!(error = %e, dir = %projects_dir.display(), "audit: create projects dir");
    }

    // Forward sync notify events into the async loop.
    let (evt_tx, mut evt_rx) = mpsc::channel::<()>(64);
    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |_res: notify::Result<notify::Event>| {
            let _ = evt_tx.blocking_send(());
        })?;
    if let Err(e) = watcher.watch(&projects_dir, RecursiveMode::Recursive) {
        tracing::warn!(
            dir = %projects_dir.display(),
            error = %e,
            "audit: watcher attach failed; audit pipeline will rely on periodic ticks only"
        );
    } else {
        tracing::info!(dir = %projects_dir.display(), "audit: watcher started");
    }

    let mut offsets: HashMap<PathBuf, u64> = HashMap::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = evt_rx.recv() => {
                scan_all(&projects_dir, &workspace_root, &mut offsets, &slot).await;
            }
            _ = tick.tick() => {
                scan_all(&projects_dir, &workspace_root, &mut offsets, &slot).await;
            }
        }
    }
}

/// One sweep of the entire `~/.claude/projects/` tree. We don't try to
/// be clever about which subdir the event came from — recursive notify
/// on macOS often coalesces events anyway, and the work per
/// already-seen file is just a `fs::metadata` size check.
///
/// Returns early without advancing any offsets if there's no active
/// hub sender. This is intentional: audit is best-effort during a hub
/// outage but should not silently drop events the moment the agent
/// reconnects.
async fn scan_all(
    projects_dir: &Path,
    workspace_root: &Path,
    offsets: &mut HashMap<PathBuf, u64>,
    slot: &SenderSlot,
) {
    let Some(tx) = slot.current() else {
        return;
    };

    let mut entries = match tokio::fs::read_dir(projects_dir).await {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = %e, "audit: read projects dir");
            return;
        }
    };
    while let Ok(Some(proj_entry)) = entries.next_entry().await {
        let proj_path = proj_entry.path();
        if !proj_entry
            .file_type()
            .await
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let mut files = match tokio::fs::read_dir(&proj_path).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(file_entry)) = files.next_entry().await {
            let path = file_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            tail_one(&path, workspace_root, offsets, &tx).await;
        }
    }
}

async fn tail_one(
    path: &Path,
    workspace_root: &Path,
    offsets: &mut HashMap<PathBuf, u64>,
    tx: &mpsc::Sender<OutFrame>,
) {
    let claude_session_id = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return,
    };
    let size = match tokio::fs::metadata(path).await {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    let offset = offsets.get(path).copied().unwrap_or(0);
    if size <= offset {
        return;
    }

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "audit: open");
            return;
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(offset)).await {
        tracing::debug!(path = %path.display(), error = %e, "audit: seek");
        return;
    }
    let mut buf = String::new();
    if let Err(e) = file.read_to_string(&mut buf).await {
        tracing::debug!(path = %path.display(), error = %e, "audit: read");
        return;
    }

    let mut bytes_advanced: u64 = 0;
    let mut send_failed = false;
    for raw in buf.split_inclusive('\n') {
        if !raw.ends_with('\n') {
            // Partial trailing line — leave it for next sweep so we
            // never ship half a JSON object.
            break;
        }
        let line_bytes = raw.len() as u64;
        let trimmed = raw.trim_end_matches('\n').trim();
        bytes_advanced += line_bytes;
        if trimmed.is_empty() {
            continue;
        }
        let json: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "audit: parse");
                continue;
            }
        };
        if let Some(frame) = classify(&json, workspace_root, &claude_session_id) {
            if tx.send(OutFrame::Text(frame)).await.is_err() {
                // Channel closed mid-batch — drop the slot, stop
                // advancing offsets so we re-ship on reconnect.
                send_failed = true;
                break;
            }
        }
    }

    if !send_failed {
        offsets.insert(path.to_path_buf(), offset + bytes_advanced);
    }
}

/// Decide whether a single parsed JSONL row should be shipped, and if
/// so build the wire frame. `None` means "drop this line".
///
/// Filters (in order):
///   1. `.type == "user"` AND `.userType == "external"` — every other
///      event type is conversation framing we don't want.
///   2. `.cwd` must sit under `workspace_root` — host claude runs
///      that happen to share `~/.claude/projects/` are ignored.
///   3. Content marker — `<bash-stdout>` / `<bash-stderr>` /
///      `<system-reminder>` prefixes are tool / system writes that
///      claude stores under `type=user` but the human didn't type;
///      drop them. `<bash-input>` is the user's bash escape; keep
///      with `kind=bash_input`. Everything else is a normal prompt.
fn classify(json: &Value, workspace_root: &Path, claude_session_id: &str) -> Option<ClientMsg> {
    if json.get("type").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    if json.get("userType").and_then(|v| v.as_str()) != Some("external") {
        return None;
    }

    let cwd_str = json.get("cwd").and_then(|v| v.as_str())?;
    let cwd_path = PathBuf::from(cwd_str);
    if !cwd_path.starts_with(workspace_root) {
        return None;
    }

    let (account, workspace) = account_workspace_from_cwd(&cwd_path, workspace_root)?;

    let raw_content = extract_content(json.get("message"));
    let trimmed = raw_content.trim_start();

    // Tool writebacks and system reminders use the same `type=user`
    // envelope as real input, so the only way to tell them apart is
    // the leading marker on the content text.
    if trimmed.starts_with("<bash-stdout>")
        || trimmed.starts_with("<bash-stderr>")
        || trimmed.starts_with("<system-reminder>")
    {
        return None;
    }
    let kind = if trimmed.starts_with("<bash-input>") {
        "bash_input"
    } else {
        "prompt"
    };

    // Strip cloudcode/claude auto-injected noise (caveat tags, prompt
    // beacons) so the audit feed shows what the user actually typed,
    // not the wrapping. If the entire content was noise, skip the
    // row — there's no real prompt to record.
    let content = clean_user_content(&raw_content)?;

    let ts_ms = json
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

    let prompt_id = json
        .get("requestId")
        .or_else(|| json.get("promptId"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let parent_uuid = json
        .get("parentUuid")
        .and_then(|v| v.as_str())
        .map(String::from);
    let git_branch = json
        .get("gitBranch")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    Some(ClientMsg::UserInteraction {
        account,
        workspace,
        claude_session_id: claude_session_id.to_string(),
        prompt_id,
        parent_uuid,
        cwd: cwd_str.to_string(),
        git_branch,
        ts_ms,
        kind: kind.to_string(),
        content,
    })
}

/// Strip cloudcode / claude auto-injected wrappers from a user
/// message so the audit feed only shows what the human actually
/// typed. Removes:
///
/// 1. Any paired XML-ish tag block — `<local-command-caveat>...</...>`,
///    `<command-name>...</...>`, `<command-message>`, `<command-args>`,
///    `<local-command-stdout>`, etc. (lazy match, supports content
///    spanning newlines.)
/// 2. The single-line prompt beacon claude prefixes to every prompt:
///    `cwd: <path>|branch: <name>|prompt: <uuid>`.
///
/// Returns `None` if nothing meaningful remains after trimming —
/// the caller should drop the whole row in that case.
fn clean_user_content(raw: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;

    static BEACON_RE: OnceLock<Regex> = OnceLock::new();
    let beacon_re = BEACON_RE.get_or_init(|| {
        // `(?m)` toggles ^/$ to per-line so we can chew a single line
        // out of a multi-line content blob without backref support.
        Regex::new(r"(?m)^[ \t]*cwd:[^\n]*\|branch:[^\n]*\|prompt:[^\n]*$\n?").unwrap()
    });

    let no_tags = strip_paired_tags(raw);
    let no_beacon = beacon_re.replace_all(&no_tags, "");
    let trimmed = no_beacon.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Tags that are *cloudcode / claude framework* output — purely
/// noise from an audit perspective. `bash-input` is **not** here
/// because that's the user's own `!command`; `bash-stdout` /
/// `bash-stderr` / `system-reminder` are caught earlier as
/// whole-message markers, so they're also off this list.
const NOISE_TAGS: &[&str] = &[
    "local-command-caveat",
    "local-command-stdout",
    "local-command-stderr",
    "command-name",
    "command-message",
    "command-args",
];

/// Remove `<tag>…</tag>` blocks for every tag in `NOISE_TAGS`.
/// Whitelist-based on purpose: a black-list would silently eat
/// `<bash-input>` (user-typed escape) or any future user-content
/// marker.  Lazy match — the first matching close terminates the
/// block, so sibling blocks don't get glued together. Unbalanced
/// opens are emitted as plain text so we never lose data on a
/// malformed input. Hand-written because `regex` is a
/// non-backtracking engine and can't backreference a specific tag.
fn strip_paired_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let Some(rel_lt) = s[i..].find('<') else {
            out.push_str(&s[i..]);
            break;
        };
        let abs_lt = i + rel_lt;
        // Flush text before the '<'.
        out.push_str(&s[i..abs_lt]);
        let after = &s[abs_lt + 1..];
        // Stray '</…>' or end-of-string — emit '<' and keep scanning.
        if after.starts_with('/') {
            out.push('<');
            i = abs_lt + 1;
            continue;
        }
        // Find the closing '>' of the open tag.
        let Some(rel_gt) = after.find('>') else {
            out.push_str(&s[abs_lt..]);
            break;
        };
        let tag = &after[..rel_gt];
        if !NOISE_TAGS.contains(&tag) {
            // Either not a tag at all (literal `<` in user text) or a
            // user-content tag (`<bash-input>`) — emit '<' and advance
            // one byte so the rest of the input keeps scanning normally.
            out.push('<');
            i = abs_lt + 1;
            continue;
        }
        let close = format!("</{}>", tag);
        let body_start = abs_lt + 1 + rel_gt + 1;
        if let Some(rel_close) = s[body_start..].find(&close) {
            // Skip the entire `<tag>…</tag>` span.
            i = body_start + rel_close + close.len();
        } else {
            // Open tag with no matching close — keep the original
            // text verbatim and bail out so we don't corrupt it.
            out.push_str(&s[abs_lt..]);
            break;
        }
    }
    out
}

/// Pull a human-readable string out of claude's `message.content`,
/// which can be either a plain string or an array of content blocks.
/// We concatenate every block's `text` field (and fall through to the
/// raw stringified block for anything we don't recognise so we never
/// lose data silently).
fn extract_content(message: Option<&Value>) -> String {
    let Some(message) = message else {
        return String::new();
    };
    let Some(content) = message.get("content") else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for block in arr {
            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        return out;
    }
    content.to_string()
}

/// Pull `(account, workspace)` out of a cwd that lives under
/// `workspace_root`. We require the cwd to be `<root>/<account>/<workspace>[/...]`;
/// shorter paths (e.g. user pointed claude directly at the root) are
/// rejected and the line is dropped.
fn account_workspace_from_cwd(cwd: &Path, workspace_root: &Path) -> Option<(String, String)> {
    let rel = cwd.strip_prefix(workspace_root).ok()?;
    let mut comps = rel.components();
    let account = comps.next()?.as_os_str().to_str()?.to_string();
    let workspace = comps.next()?.as_os_str().to_str()?.to_string();
    if account.is_empty() || workspace.is_empty() {
        return None;
    }
    Some((account, workspace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ws_root() -> PathBuf {
        PathBuf::from("/Users/me/cloudcode-agent/workspaces")
    }

    #[test]
    fn account_workspace_extraction() {
        let cwd = Path::new("/Users/me/cloudcode-agent/workspaces/alice/proj/src");
        assert_eq!(
            account_workspace_from_cwd(cwd, &ws_root()),
            Some(("alice".to_string(), "proj".to_string()))
        );
    }

    #[test]
    fn cwd_not_under_root_is_rejected() {
        let cwd = Path::new("/Users/me/other/place");
        assert_eq!(account_workspace_from_cwd(cwd, &ws_root()), None);
    }

    #[test]
    fn cwd_with_only_one_segment_is_rejected() {
        let cwd = Path::new("/Users/me/cloudcode-agent/workspaces/alice");
        assert_eq!(account_workspace_from_cwd(cwd, &ws_root()), None);
    }

    #[test]
    fn classify_keeps_a_plain_user_prompt() {
        let row = json!({
            "type": "user",
            "userType": "external",
            "cwd": "/Users/me/cloudcode-agent/workspaces/alice/proj",
            "timestamp": "2026-05-20T12:00:00Z",
            "message": { "content": "hello, please help" }
        });
        match classify(&row, &ws_root(), "sess-1") {
            Some(ClientMsg::UserInteraction { kind, content, account, workspace, .. }) => {
                assert_eq!(kind, "prompt");
                assert_eq!(content, "hello, please help");
                assert_eq!(account, "alice");
                assert_eq!(workspace, "proj");
            }
            other => panic!("expected Some(UserInteraction), got {:?}", other.is_some()),
        }
    }

    #[test]
    fn classify_keeps_bash_input() {
        let row = json!({
            "type": "user",
            "userType": "external",
            "cwd": "/Users/me/cloudcode-agent/workspaces/alice/proj",
            "timestamp": "2026-05-20T12:00:00Z",
            "message": { "content": "<bash-input>open .</bash-input>" }
        });
        match classify(&row, &ws_root(), "sess-1") {
            Some(ClientMsg::UserInteraction { kind, .. }) => assert_eq!(kind, "bash_input"),
            _ => panic!("expected bash_input"),
        }
    }

    #[test]
    fn classify_skips_tool_writebacks() {
        for marker in ["<bash-stdout>x</bash-stdout>", "<bash-stderr>x", "<system-reminder>x"] {
            let row = json!({
                "type": "user",
                "userType": "external",
                "cwd": "/Users/me/cloudcode-agent/workspaces/alice/proj",
                "message": { "content": marker }
            });
            assert!(classify(&row, &ws_root(), "s").is_none(), "{}", marker);
        }
    }

    #[test]
    fn classify_skips_non_user_rows() {
        let row = json!({
            "type": "assistant",
            "userType": "external",
            "cwd": "/Users/me/cloudcode-agent/workspaces/alice/proj",
            "message": { "content": "hi" }
        });
        assert!(classify(&row, &ws_root(), "s").is_none());
    }

    #[test]
    fn classify_skips_cwd_outside_workspace_root() {
        let row = json!({
            "type": "user",
            "userType": "external",
            "cwd": "/Users/me/random/proj",
            "message": { "content": "hi" }
        });
        assert!(classify(&row, &ws_root(), "s").is_none());
    }

    #[test]
    fn classify_handles_content_block_array() {
        let row = json!({
            "type": "user",
            "userType": "external",
            "cwd": "/Users/me/cloudcode-agent/workspaces/alice/proj",
            "message": { "content": [
                { "type": "text", "text": "part one" },
                { "type": "text", "text": "part two" }
            ] }
        });
        match classify(&row, &ws_root(), "s") {
            Some(ClientMsg::UserInteraction { content, .. }) => {
                assert_eq!(content, "part one\npart two");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn classify_strips_caveat_tag_and_beacon_keeps_user_text() {
        let raw = "    \n<local-command-caveat>Caveat: ...</local-command-caveat>\n\
                   cwd: /Users/me/cloudcode-agent/workspaces/alice/proj|branch: main|prompt: eb3dd23b-7a62-4229-8dc8-e543954c4415\n\n\
                   这是真正的问题";
        let row = json!({
            "type": "user",
            "userType": "external",
            "cwd": "/Users/me/cloudcode-agent/workspaces/alice/proj",
            "message": { "content": raw }
        });
        match classify(&row, &ws_root(), "s") {
            Some(ClientMsg::UserInteraction { content, kind, .. }) => {
                assert_eq!(content, "这是真正的问题");
                assert_eq!(kind, "prompt");
            }
            other => panic!("expected cleaned UserInteraction, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn classify_skips_when_only_noise() {
        let raw = "<local-command-caveat>x</local-command-caveat>\n\
                   cwd: /Users/me/cloudcode-agent/workspaces/alice/proj|branch: main|prompt: deadbeef\n";
        let row = json!({
            "type": "user",
            "userType": "external",
            "cwd": "/Users/me/cloudcode-agent/workspaces/alice/proj",
            "message": { "content": raw }
        });
        assert!(classify(&row, &ws_root(), "s").is_none());
    }

    #[test]
    fn clean_user_content_drops_multi_tag_blocks() {
        let raw = "<command-name>/foo</command-name>\n\
                   <command-message>foo</command-message>\n\
                   <local-command-stdout>some output</local-command-stdout>\n\
                   real prompt here";
        let got = clean_user_content(raw);
        assert_eq!(got.as_deref(), Some("real prompt here"));
    }

    #[test]
    fn clean_user_content_preserves_unrelated_text() {
        let raw = "hello world";
        assert_eq!(clean_user_content(raw).as_deref(), Some("hello world"));
    }
}
