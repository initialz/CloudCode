//! App-local mirror of the hub's `ViewerInputEvent` wire shape.
//!
//! The desktop app does NOT depend on the agent/hub crates, so this is a
//! hand-kept copy of the JSON contract. **Source of truth:**
//! `crates/hub/src/tunnel.rs` (`ViewerInputEvent`) and the parse in
//! `crates/hub/src/viewer_session.rs` (`parse_viewer_input`). Any change
//! there MUST be mirrored here; the `json_roundtrip_*` tests below pin the
//! exact serde form so drift fails the build.
//!
//! Wire shape: `#[serde(tag = "kind", rename_all = "snake_case")]` →
//! flat objects like `{"kind":"mouse_move","x":10.0,"y":20.0}`. The app
//! serializes these to the viewer ws as `Message::Text`; the hub relays
//! them verbatim to the agent.

use serde::{Deserialize, Serialize};

/// A single user-input event for the browser viewer, expressed in
/// viewport pixels (the panel does canvas→viewport scaling before
/// constructing these — that's Task 3). Field shapes copied verbatim from
/// `hub::tunnel::ViewerInputEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViewerInputEvent {
    /// Pointer moved (no button change).
    MouseMove { x: f64, y: f64 },
    /// A mouse button went down or up at `(x, y)`.
    MouseButton {
        x: f64,
        y: f64,
        /// CDP button name: `left` / `right` / `middle` / `none`.
        button: String,
        /// `true` = pressed, `false` = released.
        down: bool,
        /// CDP `clickCount` (1 = single, 2 = double, …).
        click_count: u32,
    },
    /// Scroll wheel; `dx`/`dy` are CDP deltaX/deltaY.
    Wheel { x: f64, y: f64, dx: f64, dy: f64 },
    /// A key went down or up.
    Key {
        key: String,
        code: String,
        text: String,
        down: bool,
        /// CDP modifiers bitmask (Alt=1, Ctrl=2, Meta=4, Shift=8).
        modifiers: i64,
    },
    /// Commit a whole string (IME composition end / paste).
    InsertText { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert that `ev` serializes to exactly `json` (modulo key order /
    /// whitespace — we compare parsed `serde_json::Value`s) AND that
    /// `json` deserializes back to `ev`. This pins the on-the-wire shape
    /// against the hub's `parse_viewer_input` so the two stay aligned.
    fn pin(ev: &ViewerInputEvent, json: &str) {
        let got: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(ev).unwrap()).unwrap();
        let want: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(got, want, "serialized shape drifted from hub contract");

        let parsed: ViewerInputEvent = serde_json::from_str(json).unwrap();
        assert_eq!(&parsed, ev, "deserialize roundtrip mismatch");
    }

    #[test]
    fn json_roundtrip_mouse_move() {
        pin(
            &ViewerInputEvent::MouseMove { x: 10.5, y: 20.0 },
            r#"{"kind":"mouse_move","x":10.5,"y":20.0}"#,
        );
    }

    #[test]
    fn json_roundtrip_mouse_button() {
        pin(
            &ViewerInputEvent::MouseButton {
                x: 1.0,
                y: 2.0,
                button: "left".into(),
                down: true,
                click_count: 1,
            },
            r#"{"kind":"mouse_button","x":1.0,"y":2.0,"button":"left","down":true,"click_count":1}"#,
        );
    }

    #[test]
    fn json_roundtrip_wheel() {
        pin(
            &ViewerInputEvent::Wheel {
                x: 3.0,
                y: 4.0,
                dx: -1.0,
                dy: 2.5,
            },
            r#"{"kind":"wheel","x":3.0,"y":4.0,"dx":-1.0,"dy":2.5}"#,
        );
    }

    #[test]
    fn json_roundtrip_key() {
        pin(
            &ViewerInputEvent::Key {
                key: "a".into(),
                code: "KeyA".into(),
                text: "a".into(),
                down: true,
                modifiers: 2,
            },
            r#"{"kind":"key","key":"a","code":"KeyA","text":"a","down":true,"modifiers":2}"#,
        );
    }

    #[test]
    fn json_roundtrip_insert_text() {
        pin(
            &ViewerInputEvent::InsertText { text: "你好".into() },
            r#"{"kind":"insert_text","text":"你好"}"#,
        );
    }

    /// CJK survives a full string roundtrip (the IME commit path). The hub
    /// re-emits the bytes verbatim, so all we must guarantee is that our
    /// serializer doesn't mangle multibyte UTF-8.
    #[test]
    fn cjk_insert_text_roundtrips_bytewise() {
        let ev = ViewerInputEvent::InsertText {
            text: "中文输入法测试😀".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: ViewerInputEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }
}
