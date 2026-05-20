//! Hub-specific list of optional config knobs the auto-sync engine
//! should keep documented in `hub.toml`. Each entry produces a
//! commented-out doc + assignment block at the end of the file the
//! first time a hub release introduces it. Add entries to the BOTTOM
//! so file diffs across releases stay readable.

use cloudcode_daemon::config_sync::SchemaEntry;

pub const SCHEMA: &[SchemaEntry] = &[
    SchemaEntry {
        key: "server.audit_log",
        section: "server",
        doc: &[
            "Append-only JSONL audit log of admin + agent connection events.",
            "Path is relative to the hub's working directory.",
            "Default: ./audit.jsonl",
        ],
        example: r#"audit_log = "./audit.jsonl""#,
    },
    SchemaEntry {
        key: "admin.db_path",
        section: "admin",
        doc: &[
            "SQLite file backing accounts / audit / sessions / workspaces.",
            "Default: ./cloudcode-hub.db",
        ],
        example: r#"db_path = "./cloudcode-hub.db""#,
    },
    SchemaEntry {
        key: "admin.listen",
        section: "admin",
        doc: &[
            "Admin UI listen address. Default 0.0.0.0:7101 so a fresh",
            "install is reachable; flip to 127.0.0.1 if you want SSH-",
            "tunnel-only access.",
        ],
        example: r#"listen = "0.0.0.0:7101""#,
    },
    SchemaEntry {
        key: "admin.username",
        section: "admin",
        doc: &[
            "Username required to log into the admin UI. The login form",
            "checks both username and token — a leaked token without the",
            "right username is still useless. Default: \"admin\"",
        ],
        example: r#"username = "admin""#,
    },
    SchemaEntry {
        key: "workspaces.root",
        section: "workspaces",
        doc: &[
            "Where canonical workspace bytes live on disk. Relative to",
            "the hub's working directory. Default: ./hub/workspaces",
            "Override to put workspaces on a bigger / faster volume.",
        ],
        example: r#"root = "./hub/workspaces""#,
    },
];
