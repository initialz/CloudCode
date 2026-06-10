# Desktop App P5 — 打包发布 + 收尾 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.

**Goal:** cloudcode-app 可分发:不破坏现有三件套 release、产出 macOS .app/.dmg、文档齐备。完成 = 新装机下载→打开→配 token→可用。

**约束(诚实):** 实现在远程 Linux,**无法产出/验证真 .dmg 或运行 GUI**。P5 交付:正确的 CI 配置 + 打包元数据 + 文档;真 dmg 由 CI(macos runner)产出、用户在 macOS 验收。每个任务的"验收"= 配置正确 + 现有构建不破 + 文档。

**基线:** P1-P4 完成。现有 `.github/workflows/release.yml`:tag→3 目标(2 linux musl + macos aarch64)`cargo build --release --workspace`→打包 hub/agent/cloudcode tar.gz。**风险已知**:app(eframe GUI)在 musl 上构建会失败,下次 tag 会断 release。

---

## Task 1: release CI 不被 app 破坏(必修,最高优先)

**Files:** `.github/workflows/release.yml`

- [ ] 现有三件套 tar.gz 构建改为**显式 bin**,排除 GUI app:`cargo build --release --target <t> -p cloudcode-hub -p cloudcode-agent -p cloudcode-client`(而非 `--workspace`)。这样 musl 目标不碰 cloudcode-app。
- [ ] 验证:本机 `cargo build --release -p cloudcode-hub -p cloudcode-agent -p cloudcode-client` 成功(确认显式 bin 列表正确,crate 名对)。`cargo build --release --workspace` 仍应在本机(Linux gnu,非 musl)成功——app 在 gnu Linux 能编(eframe 支持),只是 musl 不行;CI 的 musl 目标靠显式 bin 排除。
- [ ] 提交 `ci: build server binaries explicitly (exclude GUI app from musl release)`。

## Task 2: macOS .app + .dmg 打包

**Files:** `crates/app/Cargo.toml`(bundle 元数据)、`.github/workflows/release.yml`(新 job)、`crates/app/assets/`(图标占位)

- [ ] **bundle 元数据**:用 `cargo-bundle`(或 `cargo-packager`)—— 在 `crates/app/Cargo.toml` 加 `[package.metadata.bundle]`:name "CloudCode"、identifier "com.cloudcode.app"、icon(指向 assets 图标)、category。研究 cargo-bundle 当前用法(context7/docs);若 cargo-bundle 维护不活,改用 cargo-packager 或手写 .app 目录 + hdiutil 脚本——择稳。
- [ ] **图标**:放一个占位 .icns(或从一个简单 PNG 生成的脚本 `iconutil`;远程 Linux 无 iconutil,提供 PNG + 生成说明,真 .icns 在 macos CI/用户机生成)。不阻塞:bundle 可先用默认图标,文档记 TODO。
- [ ] **CI job**(macos-latest):`cargo install cargo-bundle` → `cargo bundle --release -p cloudcode-app`(产 .app)→ `hdiutil create` 或 `create-dmg` 打 .dmg → 附加到 release(`softprops/action-gh-release` files 加 `*.dmg`)。**不签名/不公证**(无证书)——文档注明用户首次打开需右键“打开”绕过 Gatekeeper(或 `xattr -dr com.apple.quarantine`)。
- [ ] **Linux app(可选,gnu tarball)**:若不费力,加一个 ubuntu job 用 gnu target 构建 cloudcode-app 打 tar.gz(eframe Linux 需 `libxkbcommon`/`libgtk`?——eframe/winit 运行时依赖,文档列出 apt 包)。费力则跳过,文档记“Linux 自行 cargo build”。
- [ ] 验证(本机能做的):`cargo metadata` 含 bundle 段无误;CI yaml 语法 `yamllint` 或 `python -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"`。真 .app/.dmg 由 CI/用户验。
- [ ] 提交 `ci: macOS .app/.dmg packaging for cloudcode-app`。

## Task 3: 文档 + 整体收尾

**Files:** `README.md`、`docs/`、根目录或 app README

- [ ] **README 桌面端段**:三种客户端(CLI / webterm / **desktop app**)介绍;app 的获取(release .dmg / 自行 `cargo run -p cloudcode-app`)、配置(同 `~/.config/cloudcode/config.toml` hub_url+token)、Gatekeeper 绕过说明、`[browser] enabled` 开启浏览器面板的前提(agent 侧装 Chrome + node)。
- [ ] **生产安全提示**:viewer token 在 ws URL query → 生产用 wss(hub 反代加 TLS);per-session 页隔离在 solo-use 下 OK、多账户共享 agent 的限制(指向 p4-page-mapping-notes)。
- [ ] **特性总览文档** `docs/superpowers/specs/` 或 README 链接:把 P1-P5 五里程碑、五份冒烟文档串成一个“桌面端特性总览”,供后续维护者入口。
- [ ] **自更新**(评估,不强求):现有 agent/client 有 update.rs 自更新;app 是否接入?P5 评估:app 可后续接同款 GitHub release 检查,但 V1 可手动下载新 dmg。文档记现状 + 后续方向(不实现,YAGNI)。
- [ ] `cargo test --workspace` 全绿、`RUSTFLAGS="-D warnings" cargo build --workspace` 零警告(含 app,在 Linux gnu)、push。
- [ ] 提交 `docs: desktop app — README, install, security, feature overview`。

## Self-Review 备忘
- T1 是真问题(app 进 workspace 后 musl release 会断),最高优先,本机可验显式 bin 构建。
- 真 dmg/GUI 不可本机验 —— P5 出配置+CI+文档,用户 macOS 验收,诚实声明(同 P3/P4 GUI 处境)。
- 不签名公证(无证书)—— 文档给 Gatekeeper 绕过,后续有证书再加。
- 自更新 YAGNI 不实现,只记方向。
- 收尾把整条特性线(spec + 5 plan + 5 smoke)文档串好,作为可维护交付物。
