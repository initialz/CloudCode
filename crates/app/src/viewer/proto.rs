//! App-local mirror of the hub's viewer wire shapes.
//!
//! The desktop app does NOT depend on the agent/hub crates, so this is a
//! hand-kept copy of the JSON contract. **Source of truth:**
//! `crates/hub/src/tunnel.rs` (`ViewerInputEvent`, `TargetInfo`) and
//! `crates/hub/src/viewer_session.rs` (`parse_viewer_uplink` for the uplink,
//! `targets_wire_json` for the downlink Text envelope). Any change there
//! MUST be mirrored here; the `json_roundtrip_*` / pinned-shape tests below
//! pin the exact serde forms so drift fails the build.
//!
//! Wire shape: `#[serde(tag = "kind", rename_all = "snake_case")]` →
//! flat objects like `{"kind":"mouse_move","x":10.0,"y":20.0}`. The app
//! serializes these to the viewer ws as `Message::Text`; the hub relays
//! them verbatim to the agent. P6 adds two more Text shapes on the same
//! `kind` tag space:
//!
//!   * downlink: `{"kind":"targets","targets":[{id,title,url,kind}]}`
//!     (hub `targets_wire_json` → [`ViewerDownlinkText::Targets`]);
//!   * uplink:   `{"kind":"select_target","target_id":"…"}`
//!     ([`select_target_json`] → hub `parse_viewer_uplink`).

use serde::{Deserialize, Serialize};

/// One CDP target (an agent-side Chrome tab) as carried in the downlink
/// `targets` envelope. Field-for-field mirror of `hub::tunnel::TargetInfo`
/// (which is what `targets_wire_json` serializes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub id: String,
    pub title: String,
    pub url: String,
    /// `"page"` for now; reserved for future kinds (e.g. electron windows).
    pub kind: String,
}

/// Downlink Text-frame envelope on the viewer ws. The hub's
/// `targets_wire_json` is the source of truth for the `Targets` shape:
/// `{"kind":"targets","targets":[…]}`. A `kind` envelope so future downlink
/// text messages can multiplex on the same socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViewerDownlinkText {
    Targets { targets: Vec<TargetInfo> },
}

/// The non-input uplink kinds (same serde conventions as
/// `ViewerInputEvent`); mirrors the hub's private `ControlUplink`.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ControlUplink<'a> {
    SelectTarget { target_id: &'a str },
    Resize { width: u32, height: u32 },
}

/// Serialize a tab-switch request into the exact uplink Text shape the
/// hub's `parse_viewer_uplink` accepts:
/// `{"kind":"select_target","target_id":"…"}`.
pub fn select_target_json(target_id: &str) -> String {
    serde_json::to_string(&ControlUplink::SelectTarget { target_id })
        .expect("select_target serialization cannot fail")
}

/// Serialize a viewport-resize request into the exact uplink Text shape the
/// hub's `parse_viewer_uplink` accepts:
/// `{"kind":"resize","width":…,"height":…}`. `width`/`height` are the panel's
/// logical (device-independent) px the agent should reflow the page to.
pub fn resize_json(width: u32, height: u32) -> String {
    serde_json::to_string(&ControlUplink::Resize { width, height })
        .expect("resize serialization cannot fail")
}

/// A single user-input event for the browser viewer, expressed in
/// viewport pixels (the panel does canvas→viewport scaling before
/// constructing these — that's Task 3). Field shapes copied verbatim from
/// `hub::tunnel::ViewerInputEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViewerInputEvent {
    /// Pointer moved. `buttons` is the CDP held-button bitmask
    /// (Left=1, Right=2, Middle=4) currently pressed — non-zero means a
    /// drag (so Chrome tracks it instead of treating it as a hover).
    /// `#[serde(default)]` keeps pre-v16 peers wire-compatible (0 = hover).
    MouseMove {
        x: f64,
        y: f64,
        #[serde(default)]
        buttons: u32,
    },
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
        /// CDP held-button bitmask AFTER this event.
        #[serde(default)]
        buttons: u32,
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
            &ViewerInputEvent::MouseMove {
                x: 10.5,
                y: 20.0,
                buttons: 1,
            },
            r#"{"kind":"mouse_move","x":10.5,"y":20.0,"buttons":1}"#,
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
                buttons: 1,
            },
            r#"{"kind":"mouse_button","x":1.0,"y":2.0,"button":"left","down":true,"click_count":1,"buttons":1}"#,
        );
    }

    /// Back-compat: a pre-v16 peer omits `buttons`; `#[serde(default)]`
    /// fills 0 (hover / no held buttons).
    #[test]
    fn mouse_events_default_buttons_when_absent() {
        let mv: ViewerInputEvent =
            serde_json::from_str(r#"{"kind":"mouse_move","x":1.0,"y":2.0}"#).unwrap();
        assert_eq!(
            mv,
            ViewerInputEvent::MouseMove {
                x: 1.0,
                y: 2.0,
                buttons: 0,
            }
        );
        let mb: ViewerInputEvent = serde_json::from_str(
            r#"{"kind":"mouse_button","x":1.0,"y":2.0,"button":"left","down":true,"click_count":1}"#,
        )
        .unwrap();
        assert_eq!(
            mb,
            ViewerInputEvent::MouseButton {
                x: 1.0,
                y: 2.0,
                button: "left".into(),
                down: true,
                click_count: 1,
                buttons: 0,
            }
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

    // --- downlink targets envelope (P6 multi-target) ----------------------

    /// The exact downlink shape `hub::viewer_session::targets_wire_json`
    /// produces (same data as its `targets_wire_shape_is_pinned` test).
    /// If the hub's wire shape changes, this constant — and the mirror
    /// types above — must change with it.
    const HUB_TARGETS_JSON: &str = r#"{"kind":"targets","targets":[{"id":"T_A","title":"百度一下","url":"https://www.baidu.com/","kind":"page"},{"id":"T_B","title":"","url":"about:blank","kind":"page"}]}"#;

    #[test]
    fn downlink_targets_roundtrips_hub_shape() {
        let parsed: ViewerDownlinkText = serde_json::from_str(HUB_TARGETS_JSON).unwrap();
        let ViewerDownlinkText::Targets { targets } = &parsed;
        assert_eq!(
            targets,
            &vec![
                TargetInfo {
                    id: "T_A".into(),
                    title: "百度一下".into(),
                    url: "https://www.baidu.com/".into(),
                    kind: "page".into(),
                },
                TargetInfo {
                    id: "T_B".into(),
                    title: "".into(),
                    url: "about:blank".into(),
                    kind: "page".into(),
                },
            ]
        );

        // Serialize back and compare as Values (key order / whitespace
        // agnostic) — pins our serde attrs to the hub's exact shape.
        let got: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        let want: serde_json::Value = serde_json::from_str(HUB_TARGETS_JSON).unwrap();
        assert_eq!(got, want, "downlink shape drifted from hub contract");
    }

    #[test]
    fn downlink_targets_empty_list_keeps_envelope() {
        // Browser idle: the hub still sends the envelope with an empty list.
        let parsed: ViewerDownlinkText =
            serde_json::from_str(r#"{"kind":"targets","targets":[]}"#).unwrap();
        let ViewerDownlinkText::Targets { targets } = &parsed;
        assert!(targets.is_empty());
    }

    #[test]
    fn downlink_unknown_kind_is_an_error() {
        // The client logs + skips these rather than tearing the ws down.
        assert!(serde_json::from_str::<ViewerDownlinkText>(r#"{"kind":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ViewerDownlinkText>("not json").is_err());
        assert!(serde_json::from_str::<ViewerDownlinkText>("{}").is_err());
    }

    // --- uplink select_target (P6 multi-target) ---------------------------

    #[test]
    fn select_target_json_exact_shape() {
        // Byte-exact: this is precisely what `hub::parse_viewer_uplink`'s
        // `uplink_parses_select_target` test feeds in.
        assert_eq!(
            select_target_json("ABC123"),
            r#"{"kind":"select_target","target_id":"ABC123"}"#
        );
    }

    #[test]
    fn resize_json_exact_shape() {
        // Byte-exact: precisely what `hub::parse_viewer_uplink`'s
        // `uplink_parses_resize` test feeds in.
        assert_eq!(
            resize_json(800, 600),
            r#"{"kind":"resize","width":800,"height":600}"#
        );
    }
}
