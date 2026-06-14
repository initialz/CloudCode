# CLAUDE.md

cloudcode 三段拓扑:**hub**(公网中继)↔ **agent**(跑 claude/tmux)↔ **client**(瘦 `cloudcode` CLI/TUI)。用户浏览器在 client,claude 在 agent。

## 架构文档(碰相关子系统前先读)

- [cc-browser 远程浏览器](docs/architecture/cc-browser.md) — 双后端(web/cc-browser)、MCP 双向协议不变量(server 反向请求必须就地应答)、超时矩阵、可观测性开关、60s 卡顿复盘。

## 工程约定

- 版本号单一真相:`Cargo.toml` 的 `[workspace.package] version`,各 crate `version.workspace = true`。SemVer。
- 发布:推 tag 触发 CI 自动发 release。线性 git 历史。
- 透明代理/桥接组件出厂即带"每帧 `方向+id+method`"边界日志(见 cc-browser 文档 §8 的元教训)。
