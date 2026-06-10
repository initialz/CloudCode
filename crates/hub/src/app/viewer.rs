//! Standalone screencast verify page, served at `/viewer`.
//!
//! One self-contained HTML document (vanilla JS, no build step) that:
//!   - reads `?session=<id>` from the URL,
//!   - opens `ws(s)://<host>/v1/viewer/ws?session=<id>` with
//!     `binaryType = "arraybuffer"`,
//!   - renders each binary JPEG frame to a `<canvas>`,
//!   - captures mouse / wheel / keyboard / IME input, scales pointer
//!     coordinates from canvas display px → viewport px, and sends each event
//!     up as a `ViewerInputEvent`-shaped JSON line (`{"kind":...}`).
//!
//! This is the P2 verification surface; the future webterm/app viewer panel
//! reuses the same protocol. Cookie auth (the `cc_user_session` set at
//! `/api/login`) rides the ws upgrade automatically, same as `/v1/pty/ws`.

use axum::response::Html;

/// `GET /viewer` — the self-contained verify page. Static; the session id is
/// read client-side from `location.search`.
pub async fn serve_viewer_html() -> Html<&'static str> {
    Html(VIEWER_HTML)
}

const VIEWER_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>cloudcode screencast viewer</title>
<style>
  html, body { margin: 0; height: 100%; background: #111; color: #ddd;
    font: 13px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; }
  #status { padding: 6px 10px; background: #1b1b1b; border-bottom: 1px solid #333;
    white-space: pre; }
  #status.ok { color: #6c6; }
  #status.err { color: #e66; }
  #wrap { display: flex; justify-content: center; align-items: flex-start;
    padding: 8px; }
  /* The canvas is sized to the incoming frame on first paint; CSS keeps it
     within the viewport while preserving aspect ratio. Input coordinates are
     scaled back to frame pixels using canvas.width / clientWidth. */
  canvas { background: #000; max-width: 100%; height: auto;
    outline: 1px solid #333; cursor: crosshair; touch-action: none; }
</style>
</head>
<body>
  <div id="status">connecting…</div>
  <div id="wrap"><canvas id="screen" width="640" height="400" tabindex="0"></canvas></div>
<script>
(function () {
  "use strict";
  var statusEl = document.getElementById("status");
  var canvas = document.getElementById("screen");
  var ctx = canvas.getContext("2d");

  function setStatus(msg, cls) {
    statusEl.textContent = msg;
    statusEl.className = cls || "";
  }

  var params = new URLSearchParams(location.search);
  var session = params.get("session");
  if (!session) {
    setStatus("missing ?session=<id> in URL", "err");
    return;
  }

  var proto = location.protocol === "https:" ? "wss:" : "ws:";
  var url = proto + "//" + location.host + "/v1/viewer/ws?session=" +
    encodeURIComponent(session);
  var ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";

  var haveFrame = false;
  ws.onopen = function () { setStatus("connected · watching " + session, "ok"); };
  ws.onclose = function (e) {
    setStatus("disconnected" + (e.reason ? " (" + e.reason + ")" : ""), "err");
  };
  ws.onerror = function () { setStatus("websocket error", "err"); };

  // ---- frame render: arraybuffer JPEG -> blob URL -> Image -> drawImage ----
  ws.onmessage = function (ev) {
    if (typeof ev.data === "string") { return; } // no text from server in P2
    var blob = new Blob([ev.data], { type: "image/jpeg" });
    var objUrl = URL.createObjectURL(blob);
    var img = new Image();
    img.onload = function () {
      // Size the canvas to the frame the first time we see one (and any time
      // the source resolution changes), so 1 canvas px == 1 viewport px and
      // the coordinate scaling below stays exact.
      if (canvas.width !== img.naturalWidth || canvas.height !== img.naturalHeight) {
        canvas.width = img.naturalWidth;
        canvas.height = img.naturalHeight;
      }
      ctx.drawImage(img, 0, 0);
      URL.revokeObjectURL(objUrl);
      if (!haveFrame) { haveFrame = true; setStatus("streaming · " +
        canvas.width + "x" + canvas.height, "ok"); }
    };
    img.onerror = function () { URL.revokeObjectURL(objUrl); };
    img.src = objUrl;
  };

  // ---- input -> JSON (ViewerInputEvent {"kind":...} form) ----
  function send(obj) {
    if (ws.readyState === WebSocket.OPEN) { ws.send(JSON.stringify(obj)); }
  }

  // Map a DOM mouse event's clientX/Y to frame (viewport) pixels. The canvas
  // is displayed at clientWidth/Height CSS px but is `canvas.width` frame px
  // wide, so scale by that ratio.
  function pt(e) {
    var r = canvas.getBoundingClientRect();
    var sx = canvas.width / r.width;
    var sy = canvas.height / r.height;
    return {
      x: (e.clientX - r.left) * sx,
      y: (e.clientY - r.top) * sy
    };
  }

  function buttonName(b) {
    return b === 2 ? "right" : b === 1 ? "middle" : b === 0 ? "left" : "none";
  }

  // CDP modifiers bitmask: Alt=1, Ctrl=2, Meta=4, Shift=8.
  function modifiers(e) {
    return (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) |
           (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0);
  }

  canvas.addEventListener("mousemove", function (e) {
    var p = pt(e);
    send({ kind: "mouse_move", x: p.x, y: p.y });
  });

  canvas.addEventListener("mousedown", function (e) {
    canvas.focus();
    var p = pt(e);
    send({ kind: "mouse_button", x: p.x, y: p.y, button: buttonName(e.button),
      down: true, click_count: e.detail || 1 });
  });

  // Listen for mouseup on window so a drag that releases off-canvas still
  // reports the button-up.
  window.addEventListener("mouseup", function (e) {
    var p = pt(e);
    send({ kind: "mouse_button", x: p.x, y: p.y, button: buttonName(e.button),
      down: false, click_count: e.detail || 1 });
  });

  canvas.addEventListener("wheel", function (e) {
    e.preventDefault();
    var p = pt(e);
    send({ kind: "wheel", x: p.x, y: p.y, dx: e.deltaX, dy: e.deltaY });
  }, { passive: false });

  // ---- keyboard + IME ----
  // While the IME is composing, keydown/keyup carry no meaningful key and the
  // committed string arrives via compositionend → insert_text. Suppress raw
  // key events during composition so we don't double-type.
  var composing = false;
  canvas.addEventListener("compositionstart", function () { composing = true; });
  canvas.addEventListener("compositionend", function (e) {
    composing = false;
    if (e.data) { send({ kind: "insert_text", text: e.data }); }
  });

  function printable(e) {
    // A single visible char (not a named key like "Enter"/"Backspace") and no
    // ctrl/meta chord → treat e.key as the text to insert.
    return e.key && e.key.length === 1 && !e.ctrlKey && !e.metaKey;
  }

  canvas.addEventListener("keydown", function (e) {
    if (composing || e.isComposing) { return; }
    e.preventDefault();
    send({ kind: "key", key: e.key, code: e.code,
      text: printable(e) ? e.key : "", down: true, modifiers: modifiers(e) });
  });
  canvas.addEventListener("keyup", function (e) {
    if (composing || e.isComposing) { return; }
    e.preventDefault();
    send({ kind: "key", key: e.key, code: e.code,
      text: printable(e) ? e.key : "", down: false, modifiers: modifiers(e) });
  });

  // Paste → insert the whole clipboard string at once (same path as IME commit).
  canvas.addEventListener("paste", function (e) {
    e.preventDefault();
    var text = (e.clipboardData || window.clipboardData).getData("text");
    if (text) { send({ kind: "insert_text", text: text }); }
  });

  // Don't let the browser's own context menu / scroll eat input over the canvas.
  canvas.addEventListener("contextmenu", function (e) { e.preventDefault(); });
})();
</script>
</body>
</html>
"##;
