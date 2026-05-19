#!/usr/bin/env bash
# v1.13 hub-managed workspace smoke test.
#
# Stands up a real hub + two agents under a temp $CLOUDCODE_STATE_DIR,
# drives the five hub-managed workspace scenarios end-to-end (pull,
# real-time push, force lock takeover, offline takeover + Welcome
# cleanup), and emits an HTML report under docs/test-reports/.
#
# Intentionally avoids any mocking: hub, agent, and the WS client
# helper (`cloudcode-smoke-ws`) are real release binaries. The only
# stub is the `claude` executable — replaced with a `tail -f /dev/null`
# wrapper so we exercise the full tmux + sandbox spawn path without
# needing a real `claude` install.
#
# Usage:
#   scripts/v1.13-smoke.sh                # run the smoke
#   scripts/v1.13-smoke.sh --rebuild-ui   # also rebuild webterm + admin-ui SPAs
#   scripts/v1.13-smoke.sh --no-build     # skip cargo build (use existing release artifacts)
#   scripts/v1.13-smoke.sh --keep-temp    # leave the temp dir on exit (for diagnosis)
#
# Exit code: 0 if all CASEs pass, 1 if any CASE fails. Hub + agent
# logs are tee'd into the temp dir so a failure leaves enough
# breadcrumbs to triage.
#
# Required tools on PATH: cargo, tmux, jq, curl, sqlite3, lsof.
# macOS only on the build side (we exercise the sandbox-exec path);
# the script itself works on Linux too but the agent will skip the
# sandbox wrapper there.

set -euo pipefail

# ---------------------------------------------------------------------
# Dep check
# ---------------------------------------------------------------------

REQUIRED=(cargo tmux jq curl sqlite3 lsof)
for t in "${REQUIRED[@]}"; do
    if ! command -v "$t" >/dev/null 2>&1; then
        echo "[FAIL] missing required tool: $t (install it and re-run; no silent fallback)" >&2
        exit 1
    fi
done

# ---------------------------------------------------------------------
# CLI args
# ---------------------------------------------------------------------

REBUILD_UI=false
SKIP_BUILD=false
KEEP_TEMP=false
for arg in "$@"; do
    case "$arg" in
        --rebuild-ui) REBUILD_UI=true ;;
        --no-build)   SKIP_BUILD=true ;;
        --keep-temp)  KEEP_TEMP=true ;;
        -h|--help)
            sed -n '/^# Usage:/,/^# Required/p' "$0" | sed 's/^# //; s/^#$//'
            exit 0
            ;;
        *) echo "[FAIL] unknown arg: $arg" >&2; exit 1 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# ---------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------

if ! $SKIP_BUILD; then
    echo "[BUILD] cargo build --release --workspace"
    cargo build --release --workspace
fi
if $REBUILD_UI; then
    echo "[BUILD] webterm (pnpm)"
    (cd webterm && pnpm install --silent && pnpm build)
    echo "[BUILD] admin-ui (npm)"
    (cd admin-ui && npm install --silent && npm run build)
    # SPAs are baked into the hub binary via rust-embed; rebuild it.
    cargo build --release -p cloudcode-hub
fi

HUB_BIN="$REPO_ROOT/target/release/cloudcode-hub"
AGENT_BIN="$REPO_ROOT/target/release/cloudcode-agent"
SMOKE_WS_BIN="$REPO_ROOT/target/release/cloudcode-smoke-ws"
for b in "$HUB_BIN" "$AGENT_BIN" "$SMOKE_WS_BIN"; do
    [ -x "$b" ] || { echo "[FAIL] missing release binary: $b" >&2; exit 1; }
done

# ---------------------------------------------------------------------
# Temp dirs
# ---------------------------------------------------------------------

TMP="$(mktemp -d -t cc-smoke)"
# Canonicalise — on macOS mktemp returns /var/... but FSEvents canonicalises
# the watch root to /private/var/..., so any path that goes into agent.toml's
# `workspace_root` or our smoke-side asserts has to be in the same form
# the notify::RecommendedWatcher sees. Otherwise watcher events get filtered
# out by `strip_prefix(root)` and the sync engine silently no-ops.
TMP="$(cd "$TMP" && pwd -P)"
echo "[INFO] temp root: $TMP"

HUB_STATE="$TMP/hub-state"
HUB_CFG="$TMP/hub.toml"
HUB_DB="$TMP/hub.sqlite"
HUB_AUDIT="$TMP/hub-audit.jsonl"
HUB_LOG="$TMP/hub.log"
HUB_WS_ROOT="$HUB_STATE/hub/workspaces"

AGENT_A_DIR="$TMP/agent-A"
AGENT_B_DIR="$TMP/agent-B"
AGENT_C_DIR="$TMP/agent-C"
mkdir -p "$AGENT_A_DIR" "$AGENT_B_DIR" "$AGENT_C_DIR" "$HUB_STATE"

REPORT_DIR="$REPO_ROOT/docs/test-reports"
REPORT_FILE="$REPORT_DIR/2026-05-19-v1.13.0-hub-managed-workspace.html"
mkdir -p "$REPORT_DIR"

# Trap-cleaned PIDs and case results
HUB_PID=""
AGENT_A_PID=""
AGENT_B_PID=""
AGENT_C_PID=""
declare -a CASE_RESULTS=()
declare -a CASE_LOGS=()
declare -a CASE_NAMES=()
declare -a CASE_DETAILS=()

cleanup() {
    set +e
    for pid in "$AGENT_A_PID" "$AGENT_B_PID" "$AGENT_C_PID" "$HUB_PID"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null
    done
    # Best-effort tmux server cleanup — the agents may have spawned
    # tmux servers under their workspace_root with labels cc-*. We
    # killed the parent agent processes which leaves the tmux servers
    # parentless but still running; sweep them by label so a re-run
    # doesn't fail with "session exists".
    for label in $(tmux ls -L cc-alice-demo 2>/dev/null | awk -F: '{print $1}' || true); do :; done
    for sock in "$AGENT_A_DIR"/* "$AGENT_B_DIR"/* "$AGENT_C_DIR"/*; do :; done
    pgrep -fl "tmux -L cc-" 2>/dev/null | awk '{print $1}' | xargs -r kill 2>/dev/null
    if $KEEP_TEMP; then
        echo "[INFO] --keep-temp set; preserving $TMP"
    else
        rm -rf "$TMP"
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------

log_case() {
    # log_case <case> <line>
    local c="$1"; shift
    echo "[$c] $*"
    CASE_LOGS[${#CASE_LOGS[@]}]="[$c] $*"
}

fail_case() {
    # fail_case <case-name> <reason>
    local name="$1"; shift
    local reason="$*"
    echo "[FAIL] $name: $reason" >&2
    CASE_RESULTS+=("FAIL")
    CASE_NAMES+=("$name")
    CASE_DETAILS+=("$reason")
    # Always tail hub log on failure to make triage local.
    echo "---- hub log tail ----" >&2
    tail -30 "$HUB_LOG" >&2 || true
    echo "---- agent-A log tail ----" >&2
    tail -30 "$AGENT_A_DIR/agent.log" 2>/dev/null >&2 || true
    echo "---- agent-B log tail ----" >&2
    tail -30 "$AGENT_B_DIR/agent.log" 2>/dev/null >&2 || true
    return 1
}

pass_case() {
    # pass_case <case-name> <detail>
    CASE_RESULTS+=("PASS")
    CASE_NAMES+=("$1")
    CASE_DETAILS+=("$2")
    echo "[OK] $1"
}

pick_port() {
    # Ask the kernel for an ephemeral port via python (always present
    # on macOS); fall back to a fixed unused range.
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_for_url() {
    # wait_for_url <url> <timeout-secs>
    local url="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        if curl -sf "$url" >/dev/null 2>&1; then return 0; fi
        sleep 0.2
    done
    return 1
}

wait_for_agent_online() {
    # wait_for_agent_online <name> <timeout-iters at 200ms each>
    # Uses /admin/api/dashboard's online_agents list to avoid the
    # /admin/api/agents path which triggers a GitHub releases fetch
    # (slow on a fresh boot, 15s timeout).
    local name="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        if curl -sf -b "$ADMIN_COOKIE" "$ADMIN_URL/admin/api/dashboard" \
            | jq -e --arg n "$name" '.online_agents | index($n)' >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

wait_for_agent_offline() {
    # wait_for_agent_offline <name> <timeout-iters at 200ms each>
    local name="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        if ! curl -sf -b "$ADMIN_COOKIE" "$ADMIN_URL/admin/api/dashboard" \
            | jq -e --arg n "$name" '.online_agents | index($n)' >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# ---------------------------------------------------------------------
# Hub bring-up
# ---------------------------------------------------------------------

HUB_PORT="$(pick_port)"
ADMIN_PORT="$(pick_port)"
HUB_URL="http://127.0.0.1:$HUB_PORT"
ADMIN_URL="http://127.0.0.1:$ADMIN_PORT"
HUB_WS="ws://127.0.0.1:$HUB_PORT/v1/pty/ws"
AGENT_WS="ws://127.0.0.1:$HUB_PORT/v1/agent/ws"
ADMIN_COOKIE="$TMP/admin-cookie.jar"

# Generate hub.toml + capture the agent registration token + admin token
echo "[INFO] generating hub.toml"
HUB_INIT_OUT="$TMP/hub-init.out"
"$HUB_BIN" --init --config "$HUB_CFG" >"$HUB_INIT_OUT" 2>&1
AGENT_TOKEN="$(grep -E '^ag_[a-f0-9]+$' "$HUB_INIT_OUT" | head -1)"
ADMIN_TOKEN="$(grep -E '^ad_[a-f0-9]+$' "$HUB_INIT_OUT" | head -1)"
if [ -z "$AGENT_TOKEN" ] || [ -z "$ADMIN_TOKEN" ]; then
    echo "[FAIL] could not extract agent/admin tokens from hub --init output" >&2
    cat "$HUB_INIT_OUT" >&2
    exit 1
fi

# Patch the generated hub.toml: bind to our random localhost ports,
# point DB + audit at the temp dir.
python3 - "$HUB_CFG" "$HUB_PORT" "$ADMIN_PORT" "$HUB_DB" "$HUB_AUDIT" <<'PY'
import sys, re
path, hub_port, admin_port, db_path, audit_path = sys.argv[1:6]
src = open(path).read()
src = re.sub(r'^listen = "0\.0\.0\.0:7100"', f'listen = "127.0.0.1:{hub_port}"', src, count=1, flags=re.M)
src = re.sub(r'^listen = "0\.0\.0\.0:7101"', f'listen = "127.0.0.1:{admin_port}"', src, count=1, flags=re.M)
src = re.sub(r'audit_log = "\./audit\.jsonl"', f'audit_log = "{audit_path}"', src, count=1)
src = re.sub(r'db_path = "\./cloudcode-hub\.db"', f'db_path = "{db_path}"', src, count=1)
open(path, "w").write(src)
PY

echo "[INFO] starting hub on $HUB_URL (admin $ADMIN_URL)"
(
    CLOUDCODE_STATE_DIR="$HUB_STATE" \
    RUST_LOG="info,cloudcode_hub=debug" \
        "$HUB_BIN" --config "$HUB_CFG" >"$HUB_LOG" 2>&1 &
    echo $! >"$TMP/hub.pid"
)
HUB_PID="$(cat "$TMP/hub.pid")"

if ! wait_for_url "$HUB_URL/healthz" 100; then
    echo "[FAIL] hub failed to come up within 20s" >&2
    cat "$HUB_LOG" >&2
    exit 1
fi
echo "[OK] hub up (pid $HUB_PID)"

# ---------------------------------------------------------------------
# Admin login + bootstrap (account + agent ACLs + sandbox off)
# ---------------------------------------------------------------------

curl -sf -c "$ADMIN_COOKIE" -H 'content-type: application/json' \
    -d "$(jq -nc --arg t "$ADMIN_TOKEN" '{token:$t}')" \
    "$ADMIN_URL/admin/api/login" >/dev/null \
    || { echo "[FAIL] admin login"; exit 1; }
echo "[OK] admin login"

# Create the test account, capture its plaintext token (one-shot).
CREATE_ACCT="$(curl -sf -b "$ADMIN_COOKIE" -H 'content-type: application/json' \
    -d '{"name":"alice"}' "$ADMIN_URL/admin/api/accounts")"
ALICE_TOKEN="$(echo "$CREATE_ACCT" | jq -r .token)"
[ -n "$ALICE_TOKEN" ] && [ "$ALICE_TOKEN" != "null" ] \
    || { echo "[FAIL] account create"; echo "$CREATE_ACCT"; exit 1; }
echo "[OK] account 'alice' created"

# Allow the three smoke agents for alice. Pre-allow before they connect
# so OpenSession's ACL check passes the moment the agent comes up.
curl -sf -b "$ADMIN_COOKIE" -X PUT -H 'content-type: application/json' \
    -d '{"agents":["smoke-agent-A","smoke-agent-B","smoke-agent-C"]}' \
    "$ADMIN_URL/admin/api/accounts/alice/allowed-agents" \
    || { echo "[FAIL] allowlist set"; exit 1; }

# Disable sandbox for alice — the smoke exercises file sync, not the
# sandbox profile, and skipping the wrapper makes failures easier
# to read in agent logs. Default for new accounts is sandbox_enabled=1,
# so one toggle flips it off.
curl -sf -b "$ADMIN_COOKIE" -X POST \
    "$ADMIN_URL/admin/api/accounts/alice/sandbox" \
    || { echo "[FAIL] sandbox toggle"; exit 1; }
echo "[OK] alice ACL set, sandbox off"

# ---------------------------------------------------------------------
# Agent setup
# ---------------------------------------------------------------------

write_agent_config() {
    # write_agent_config <dir> <name>
    local dir="$1" name="$2"
    # Fake claude wrapper: keep tmux alive so PtyOpened actually fires
    # and the sync watcher has time to observe filesystem changes.
    cat >"$dir/fake-claude" <<'STUB'
#!/usr/bin/env bash
# Fake `claude` for the v1.13 smoke. Stays alive (and quiet) so the
# tmux session it lives in stays attached and the agent sync engine
# has a stable cwd to watch.
echo "[fake claude up] argv: $*"
exec tail -f /dev/null
STUB
    chmod +x "$dir/fake-claude"

    cat >"$dir/agent.toml" <<TOML
[hub]
url = "$AGENT_WS"

[agent]
name = "$name"

[auth]
registration_token = "$AGENT_TOKEN"

[claude]
workspace_root = "$dir/workspaces"

[tools]
default = "claude"

[tools.claude]
executable = "$dir/fake-claude"
resume_command = ""
extra_args = []
TOML
    mkdir -p "$dir/workspaces" "$dir/state"
}

start_agent() {
    # start_agent <dir> <name> -> writes pid file to <dir>/agent.pid
    local dir="$1" name="$2"
    (
        CLOUDCODE_STATE_DIR="$dir/state" \
        RUST_LOG="info,cloudcode_agent=debug" \
            "$AGENT_BIN" --config "$dir/agent.toml" >"$dir/agent.log" 2>&1 &
        echo $! >"$dir/agent.pid"
    )
}

write_agent_config "$AGENT_A_DIR" "smoke-agent-A"
write_agent_config "$AGENT_B_DIR" "smoke-agent-B"
write_agent_config "$AGENT_C_DIR" "smoke-agent-C"

start_agent "$AGENT_A_DIR" "smoke-agent-A"
AGENT_A_PID="$(cat "$AGENT_A_DIR/agent.pid")"
start_agent "$AGENT_B_DIR" "smoke-agent-B"
AGENT_B_PID="$(cat "$AGENT_B_DIR/agent.pid")"

if ! wait_for_agent_online smoke-agent-A 100; then
    echo "[FAIL] agent-A did not come online" >&2
    tail -30 "$AGENT_A_DIR/agent.log" >&2
    exit 1
fi
if ! wait_for_agent_online smoke-agent-B 100; then
    echo "[FAIL] agent-B did not come online" >&2
    tail -30 "$AGENT_B_DIR/agent.log" >&2
    exit 1
fi
echo "[OK] agents A + B online"

smoke_ws() {
    # smoke_ws --token <tok> <subcommand args...>
    "$SMOKE_WS_BIN" --url "$HUB_WS" "$@"
}

# ---------------------------------------------------------------------
# CASE 1 — create workspace + seed canonical README.md
# ---------------------------------------------------------------------

echo "[CASE 1] create workspace + seed canonical content"
CASE1_LOG="$TMP/case1.log"
{
    smoke_ws --token "$ALICE_TOKEN" create-workspace --name demo
} >"$CASE1_LOG" 2>&1 || { fail_case "CASE 1" "CreateWorkspace failed: $(cat "$CASE1_LOG")"; }

# Hub-canonical dir should now exist at:
CANON="$HUB_WS_ROOT/alice/demo"
if [ ! -d "$CANON" ]; then
    fail_case "CASE 1" "canonical dir $CANON missing after CreateWorkspace"
fi
echo "hello v1.13" >"$CANON/README.md"
log_case "CASE 1" "wrote $CANON/README.md = 'hello v1.13'"

# Verify ListWorkspaces returns it.
LIST_OUT="$(smoke_ws --token "$ALICE_TOKEN" list-workspaces 2>"$CASE1_LOG.list")"
if ! echo "$LIST_OUT" | jq -e '.items[] | select(.name == "demo")' >/dev/null; then
    fail_case "CASE 1" "demo not in ListWorkspaces reply: $LIST_OUT"
fi
pass_case "CASE 1" "Workspace 'demo' created and seeded with README.md"

# ---------------------------------------------------------------------
# CASE 2 — agent-A opens a session; canonical bytes get pulled
# ---------------------------------------------------------------------

echo "[CASE 2] agent-A OpenSession should pull README.md into its local copy"
CASE2_LOG="$TMP/case2.log"
# hold-secs lets the workspace stay locked long enough for CASE 3 to push.
smoke_ws --token "$ALICE_TOKEN" open-session \
    --workspace demo --agent smoke-agent-A --hold-secs 15 \
    >"$CASE2_LOG.out" 2>&1 &
CASE2_PID=$!

# Give the agent up to 10s to write README.md.
A_README="$AGENT_A_DIR/workspaces/alice/demo/README.md"
for _ in $(seq 1 50); do
    [ -f "$A_README" ] && break
    sleep 0.2
done
if [ ! -f "$A_README" ]; then
    kill "$CASE2_PID" 2>/dev/null || true
    fail_case "CASE 2" "$A_README not materialised after pull"
fi
if ! grep -q "hello v1.13" "$A_README"; then
    kill "$CASE2_PID" 2>/dev/null || true
    fail_case "CASE 2" "$A_README content mismatch: $(cat "$A_README")"
fi
log_case "CASE 2" "agent-A pulled README.md ($(wc -c <"$A_README") bytes)"
pass_case "CASE 2" "OpenSession streamed README.md from hub canonical → agent-A working copy"

# ---------------------------------------------------------------------
# CASE 3 — agent-A edits → hub canonical reflects the change
# ---------------------------------------------------------------------

echo "[CASE 3] agent-A edit triggers hub-side push within COALESCE_WINDOW"

# Append a line on the agent side; expect the hub canonical copy to
# see it within a couple of seconds. (100ms coalesce + 500ms scan
# backstop + WS round-trip.)
echo "edited by agent-A" >>"$A_README"
NEW_FILE="$AGENT_A_DIR/workspaces/alice/demo/NEW_FILE.md"
echo "fresh file" >"$NEW_FILE"
log_case "CASE 3" "appended to README.md, created NEW_FILE.md on agent-A"

for _ in $(seq 1 50); do
    if grep -q "edited by agent-A" "$CANON/README.md" 2>/dev/null \
        && [ -f "$CANON/NEW_FILE.md" ]; then
        break
    fi
    sleep 0.2
done
if ! grep -q "edited by agent-A" "$CANON/README.md" 2>/dev/null; then
    kill "$CASE2_PID" 2>/dev/null || true
    fail_case "CASE 3" "hub canonical README.md did not see edit: $(cat "$CANON/README.md")"
fi
if [ ! -f "$CANON/NEW_FILE.md" ]; then
    kill "$CASE2_PID" 2>/dev/null || true
    fail_case "CASE 3" "hub canonical NEW_FILE.md never appeared"
fi
log_case "CASE 3" "hub canonical mirrors push"

# Delete on agent side → hub canonical drops it.
rm "$NEW_FILE"
for _ in $(seq 1 50); do
    [ ! -f "$CANON/NEW_FILE.md" ] && break
    sleep 0.2
done
if [ -f "$CANON/NEW_FILE.md" ]; then
    kill "$CASE2_PID" 2>/dev/null || true
    fail_case "CASE 3" "hub canonical NEW_FILE.md still present after agent-side delete"
fi
pass_case "CASE 3" "Real-time push + delete propagate to hub canonical"

# ---------------------------------------------------------------------
# CASE 4 — agent-B force-takes the lock from agent-A
# ---------------------------------------------------------------------

echo "[CASE 4] agent-B OpenSession with force=true should evict agent-A's copy"
CASE4_LOG="$TMP/case4.log"
smoke_ws --token "$ALICE_TOKEN" open-session \
    --workspace demo --agent smoke-agent-B --force --hold-secs 12 \
    >"$CASE4_LOG.out" 2>&1 &
CASE4_PID=$!

# Wait up to 10s for agent-A's local dir to be cleaned up. Agent-A
# is still connected (its CASE 2 hold-secs is still running) so the
# WorkspaceCleanup arrives in-band rather than via Welcome drain.
A_WS="$AGENT_A_DIR/workspaces/alice/demo"
for _ in $(seq 1 50); do
    [ ! -d "$A_WS" ] && break
    sleep 0.2
done
if [ -d "$A_WS" ]; then
    kill "$CASE2_PID" "$CASE4_PID" 2>/dev/null || true
    fail_case "CASE 4" "agent-A's local copy $A_WS still present after force-takeover"
fi

# And the new lock holder is agent-B.
LOCK_HOLDER="$(sqlite3 "$HUB_DB" \
    "SELECT locked_by_agent FROM workspaces WHERE account='alice' AND name='demo';")"
if [ "$LOCK_HOLDER" != "smoke-agent-B" ]; then
    kill "$CASE2_PID" "$CASE4_PID" 2>/dev/null || true
    fail_case "CASE 4" "lock holder is '$LOCK_HOLDER', expected smoke-agent-B"
fi
log_case "CASE 4" "lock holder=$LOCK_HOLDER, agent-A local copy cleared"

# Reap CASE 2/4 holders so the workspace is free for CASE 5.
wait "$CASE2_PID" 2>/dev/null || true
wait "$CASE4_PID" 2>/dev/null || true
pass_case "CASE 4" "force=true successfully transferred lock from agent-A to agent-B"

# ---------------------------------------------------------------------
# CASE 5 — offline force-take: B suspended → C force-takes (queues
#           cleanup) → B killed/restarted → drains cleanup on Welcome.
#
# Subtlety the user should know about: when the hub's WS reader loop
# sees an agent's TCP connection drop, it calls
# `release_all_workspace_locks_for_agent` (see ws_handler.rs:213) and
# the workspace becomes free, so a subsequent `open_session` from a
# different agent doesn't even need force=true and — critically —
# does NOT queue a pending cleanup for the old holder. The
# "force-take from a dead agent" pending-cleanup path only fires
# while the hub still believes the old agent is alive.
#
# To exercise that path reliably we SIGSTOP agent-B (its TCP socket
# stays open; the kernel keeps ACKing pings, the userspace just
# never reads). C then force-takes and the queue fires. Finally we
# SIGKILL B, restart it from scratch, and watch the Welcome-drain
# remove its stale local copy.
# ---------------------------------------------------------------------

echo "[CASE 5] suspend B → C force-takes → restart B drains on Welcome"

# Pre-seed agent-B with extra content under the workspace so we can
# observe its cleanup (the canonical copy already has README.md, but
# adding STALE.md here gives us an unambiguous "this was the dead
# agent's copy" marker).
mkdir -p "$AGENT_B_DIR/workspaces/alice/demo"
echo "stale local copy" >"$AGENT_B_DIR/workspaces/alice/demo/STALE.md"

# Freeze agent-B so the hub still thinks it owns the workspace lock.
# Without this, the kill -9 below would race against the C OpenSession
# and the hub would see B drop *before* C grabs the lock, releasing
# the lock and skipping the queue_pending_cleanup branch in
# pty_session.rs::open_session.
kill -STOP "$AGENT_B_PID"
log_case "CASE 5" "agent-B SIGSTOPped (pid $AGENT_B_PID); WS stays half-alive"

# Start agent-C and force-take. While B is frozen the hub still sees
# it as the lock holder, so C's force=true triggers the queue.
start_agent "$AGENT_C_DIR" "smoke-agent-C"
AGENT_C_PID="$(cat "$AGENT_C_DIR/agent.pid")"
wait_for_agent_online smoke-agent-C 100 \
    || { kill -CONT "$AGENT_B_PID" 2>/dev/null; fail_case "CASE 5" "agent-C did not come online"; }

CASE5_LOG="$TMP/case5.log"
smoke_ws --token "$ALICE_TOKEN" open-session \
    --workspace demo --agent smoke-agent-C --force --hold-secs 10 \
    >"$CASE5_LOG.out" 2>&1 &
CASE5_PID=$!

# Wait for C to take the lock + the pending row to appear.
for _ in $(seq 1 50); do
    LOCK_HOLDER="$(sqlite3 "$HUB_DB" \
        "SELECT locked_by_agent FROM workspaces WHERE account='alice' AND name='demo';")"
    [ "$LOCK_HOLDER" = "smoke-agent-C" ] && break
    sleep 0.2
done
if [ "$LOCK_HOLDER" != "smoke-agent-C" ]; then
    kill "$CASE5_PID" 2>/dev/null || true
    kill -CONT "$AGENT_B_PID" 2>/dev/null || true
    fail_case "CASE 5" "agent-C did not take lock: holder=$LOCK_HOLDER"
fi

PENDING_COUNT=$(sqlite3 "$HUB_DB" \
    "SELECT COUNT(*) FROM pending_workspace_cleanups
       WHERE agent='smoke-agent-B' AND account='alice' AND workspace='demo';")
if [ "$PENDING_COUNT" -lt 1 ]; then
    kill "$CASE5_PID" 2>/dev/null || true
    kill -CONT "$AGENT_B_PID" 2>/dev/null || true
    fail_case "CASE 5" "no pending cleanup queued for frozen agent-B"
fi
log_case "CASE 5" "agent-C took lock; pending_workspace_cleanups has $PENDING_COUNT row for B"

# Now actually kill the frozen agent so we can start a fresh process
# with the same name + same workspace_root. Send SIGCONT first so the
# tokio runtime can run its drop handlers; then SIGKILL.
kill -CONT "$AGENT_B_PID" 2>/dev/null || true
kill -9 "$AGENT_B_PID" 2>/dev/null || true
wait "$AGENT_B_PID" 2>/dev/null || true
AGENT_B_PID=""
wait_for_agent_offline smoke-agent-B 150 \
    || { kill "$CASE5_PID" 2>/dev/null || true; fail_case "CASE 5" "hub didn't register B as offline after kill -9"; }

# Stale local copy must still be there — only the agent itself (via
# the Welcome-drain WorkspaceCleanup) should remove it.
[ -f "$AGENT_B_DIR/workspaces/alice/demo/STALE.md" ] \
    || fail_case "CASE 5" "STALE.md disappeared before reconnect (unexpected)"

# Restart agent-B; it should drain the pending cleanup on Welcome.
start_agent "$AGENT_B_DIR" "smoke-agent-B"
AGENT_B_PID="$(cat "$AGENT_B_DIR/agent.pid")"
wait_for_agent_online smoke-agent-B 100 \
    || { kill "$CASE5_PID" 2>/dev/null || true; fail_case "CASE 5" "agent-B did not reconnect"; }

# Within ~10s, B's local copy should be rm -rf'd.
B_WS="$AGENT_B_DIR/workspaces/alice/demo"
for _ in $(seq 1 50); do
    [ ! -d "$B_WS" ] && break
    sleep 0.2
done
if [ -d "$B_WS" ]; then
    kill "$CASE5_PID" 2>/dev/null || true
    fail_case "CASE 5" "agent-B did not clean up stale copy after Welcome"
fi
log_case "CASE 5" "agent-B Welcome drain removed $B_WS"

PENDING_AFTER=$(sqlite3 "$HUB_DB" \
    "SELECT COUNT(*) FROM pending_workspace_cleanups
       WHERE agent='smoke-agent-B' AND account='alice' AND workspace='demo';")
if [ "$PENDING_AFTER" -ne 0 ]; then
    kill "$CASE5_PID" 2>/dev/null || true
    fail_case "CASE 5" "pending_workspace_cleanups still has $PENDING_AFTER rows for B"
fi

wait "$CASE5_PID" 2>/dev/null || true
pass_case "CASE 5" "Frozen-holder force-take + Welcome-drain cleanup work end-to-end"

# ---------------------------------------------------------------------
# Tally + HTML report
# ---------------------------------------------------------------------

PASS_COUNT=0
FAIL_COUNT=0
for r in "${CASE_RESULTS[@]}"; do
    [ "$r" = "PASS" ] && PASS_COUNT=$((PASS_COUNT + 1)) || FAIL_COUNT=$((FAIL_COUNT + 1))
done

echo
echo "===== SMOKE: $PASS_COUNT PASS, $FAIL_COUNT FAIL ====="

# Build HTML report. Inline CSS only — matches the v1.12 report style.
HUB_LOG_TAIL=$(tail -120 "$HUB_LOG" 2>/dev/null | python3 -c 'import sys,html;print(html.escape(sys.stdin.read()))')
AGENT_A_LOG_TAIL=$(tail -120 "$AGENT_A_DIR/agent.log" 2>/dev/null | python3 -c 'import sys,html;print(html.escape(sys.stdin.read()))')
AGENT_B_LOG_TAIL=$(tail -120 "$AGENT_B_DIR/agent.log" 2>/dev/null | python3 -c 'import sys,html;print(html.escape(sys.stdin.read()))')

{
cat <<HTML
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>CloudCode v1.13.0 — Hub-managed workspace smoke</title>
<style>
  :root {
    --bg: #fafafa; --fg: #18181b; --muted: #71717a; --border: #e4e4e7;
    --card: #ffffff; --pass: #15803d; --pass-bg: #dcfce7;
    --fail: #b91c1c; --fail-bg: #fee2e2; --warn: #b45309; --warn-bg: #fef3c7;
    --code-bg: #18181b; --code-fg: #fafafa; --hl: #2563eb;
  }
  @media (prefers-color-scheme: dark) {
    :root { --bg: #09090b; --fg: #fafafa; --muted: #a1a1aa; --border: #27272a; --card: #18181b; }
  }
  * { box-sizing: border-box; }
  body { margin: 0; padding: 0; background: var(--bg); color: var(--fg);
    font: 15px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif; }
  .page { max-width: 1000px; margin: 0 auto; padding: 40px 32px 80px; }
  header.report { border-bottom: 1px solid var(--border); padding-bottom: 24px; margin-bottom: 32px; }
  header.report h1 { margin: 0 0 6px; font-size: 26px; letter-spacing: -0.01em; }
  header.report .meta { color: var(--muted); font-size: 13px; font-variant-numeric: tabular-nums; }
  header.report .scope { margin-top: 14px; color: var(--muted); line-height: 1.6; }
  h2 { margin: 40px 0 12px; font-size: 19px; letter-spacing: -0.005em; }
  h3 { margin: 18px 0 8px; font-size: 15px; color: var(--muted); text-transform: uppercase; letter-spacing: 0.06em; font-weight: 600; }
  p { margin: 8px 0; }
  table { width: 100%; border-collapse: collapse; margin: 12px 0; font-size: 14px; }
  th, td { text-align: left; padding: 8px 12px; border-bottom: 1px solid var(--border); vertical-align: top; }
  th { color: var(--muted); font-weight: 500; font-size: 12px; text-transform: uppercase; letter-spacing: 0.06em; }
  td.mono { font-family: ui-monospace, Menlo, monospace; font-size: 13px; }
  pre.log { background: var(--code-bg); color: var(--code-fg); border-radius: 8px;
    padding: 16px 18px; overflow-x: auto; font: 12.5px/1.55 ui-monospace, Menlo, monospace; margin: 12px 0; max-height: 320px; }
  .badge { display: inline-block; padding: 2px 10px; border-radius: 999px; font-size: 11px;
    font-weight: 600; letter-spacing: 0.03em; text-transform: uppercase; font-family: ui-monospace, Menlo, monospace; }
  .badge.pass { background: var(--pass-bg); color: var(--pass); }
  .badge.fail { background: var(--fail-bg); color: var(--fail); }
  .verdict { margin: 14px 0 0; padding: 12px 14px; border-radius: 8px;
    border-left: 3px solid var(--pass); background: var(--pass-bg); color: var(--pass); font-size: 13px; }
  .verdict.fail { border-left-color: var(--fail); background: var(--fail-bg); color: var(--fail); }
  ul.checks { margin: 10px 0; padding-left: 0; list-style: none; }
  ul.checks li { padding: 4px 0; }
  ul.checks li::before { content: "✓"; color: var(--pass); font-weight: 700; margin-right: 8px; }
  ul.checks li.fail::before { content: "✗"; color: var(--fail); }
  footer.report { margin-top: 64px; padding-top: 24px; border-top: 1px solid var(--border); color: var(--muted); font-size: 12px; }
  code.inline { background: var(--card); border: 1px solid var(--border); border-radius: 4px; padding: 1px 6px;
    font: 12.5px/1.4 ui-monospace, Menlo, monospace; }
</style>
</head>
<body>
<div class="page">
<header class="report">
  <h1>CloudCode v1.13.0 — Hub-managed workspace smoke</h1>
  <div class="meta">
    Date: 2026-05-19 &nbsp;·&nbsp;
    Branch: $(git rev-parse --abbrev-ref HEAD) &nbsp;·&nbsp;
    Commit: $(git rev-parse --short HEAD) &nbsp;·&nbsp;
    Host: $(uname -s) $(uname -m) &nbsp;·&nbsp;
    Verdict: <span class="badge $([ "$FAIL_COUNT" -eq 0 ] && echo pass || echo fail)">$PASS_COUNT/${#CASE_RESULTS[@]} PASSED</span>
  </div>
  <div class="scope">
    Exercises the v1.13 split between hub-canonical workspace bytes and the agent's working copy:
    initial pull, real-time push, force-take, and offline force-take with Welcome-drain cleanup.
    Hub, agent, and WS client are all real release binaries; the only stub is <code class="inline">claude</code>,
    replaced with a <code class="inline">tail -f /dev/null</code> wrapper so we exercise the full
    tmux spawn path without needing a real claude install.
  </div>
</header>

<h2>Environment + isolation</h2>
<table>
  <tr><th>State dir</th><td class="mono">$TMP</td></tr>
  <tr><th>Hub listen</th><td class="mono">127.0.0.1:$HUB_PORT (PTY/agent), 127.0.0.1:$ADMIN_PORT (admin)</td></tr>
  <tr><th>Hub canonical store</th><td class="mono">$HUB_WS_ROOT</td></tr>
  <tr><th>Agent A workspace_root</th><td class="mono">$AGENT_A_DIR/workspaces</td></tr>
  <tr><th>Agent B workspace_root</th><td class="mono">$AGENT_B_DIR/workspaces</td></tr>
  <tr><th>Agent C workspace_root</th><td class="mono">$AGENT_C_DIR/workspaces</td></tr>
</table>
<p>The script writes nothing outside <code class="inline">$TMP</code> and
<code class="inline">docs/test-reports/</code>. The user's
<code class="inline">~/.local/state/cloudcode</code> is never touched (every process boots with
<code class="inline">CLOUDCODE_STATE_DIR=$TMP/...</code>).</p>
HTML

for i in "${!CASE_RESULTS[@]}"; do
    name="${CASE_NAMES[$i]}"
    result="${CASE_RESULTS[$i]}"
    detail="${CASE_DETAILS[$i]}"
    badge_class="pass"
    verdict_class=""
    [ "$result" = "FAIL" ] && { badge_class="fail"; verdict_class=" fail"; }
    cat <<HTML
<h2>$name <span class="badge $badge_class">$result</span></h2>
<div class="verdict$verdict_class">$detail</div>
HTML
done

CASE_LOG_BLOCK="$(printf '%s\n' "${CASE_LOGS[@]}" | python3 -c 'import sys,html;print(html.escape(sys.stdin.read()))')"
cat <<HTML

<h2>Script log</h2>
<pre class="log">$CASE_LOG_BLOCK</pre>

<h2>Hub log (tail)</h2>
<pre class="log">$HUB_LOG_TAIL</pre>

<h2>Agent A log (tail)</h2>
<pre class="log">$AGENT_A_LOG_TAIL</pre>

<h2>Agent B log (tail)</h2>
<pre class="log">$AGENT_B_LOG_TAIL</pre>

<h2>Out of scope</h2>
<ul class="checks">
  <li>Sandbox profile correctness (sandbox is toggled OFF for the smoke account)</li>
  <li>Webterm SPA integration — covered by the manual Phase B checklist</li>
  <li>Self-update / supervisor flows — covered by v1.11 release report</li>
  <li>Cross-version compat (old client → new hub, etc.) — Phase B</li>
</ul>

<footer class="report">
  Generated by scripts/v1.13-smoke.sh on $(date -Iseconds)
</footer>
</div>
</body>
</html>
HTML
} >"$REPORT_FILE"

echo "[INFO] HTML report: $REPORT_FILE"

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi
