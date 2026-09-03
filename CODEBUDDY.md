# CODEBUDDY.md This file provides guidance to CodeBuddy when working with code in this repository.

## 常用开发命令

> 以仓库根目录 `/Users/atom/worksapce/easytier/EasyTier` 为工作目录。

- `pnpm -r install`  
  安装 monorepo 前端依赖（GUI、`easytier-web/frontend`、`frontend-lib`、插件）。

- `cargo build --release`  
  构建 Rust workspace 默认成员（`easytier` 与 `easytier-web`）。

- `cargo build --release --target x86_64-apple-darwin`  
  按目标三元组交叉/定向构建；CI 也使用同类命令构建多平台产物。

- `pnpm -r --filter "./easytier-web/*" build`  
  构建 Web 控制台相关前端包（`frontend` 与 `frontend-lib`）。

- `cd /Users/atom/worksapce/easytier/EasyTier/easytier-gui && pnpm tauri dev`  
  启动桌面 GUI 开发模式（Tauri + Vue）。

- `cd /Users/atom/worksapce/easytier/EasyTier/easytier-gui && pnpm tauri build`  
  构建桌面 GUI 发布包。

- `cargo fmt --all -- --check`  
  Rust 格式检查（CI 强约束）。

- `cargo clippy --all-targets --features full --all -- -D warnings`  
  Rust 静态检查，按 CI 配置将告警视为错误。

- `cargo hack check --package easytier --each-feature --features aes-gcm --exclude-features macos-ne --verbose`  
  对 `easytier` 做 feature 组合可编译性检查。

- `cargo test --no-default-features --features=full --verbose`  
  按贡献文档执行核心测试。

- `cargo install cargo-nextest --locked && cargo nextest run -p easytier --features full`  
  使用 nextest 运行 `easytier` 测试；与 CI 体系一致。

- `cargo nextest run -p easytier -E 'test(subnet_proxy_three_node_test)' --test-threads 1 --no-fail-fast`  
  运行单个关键测试（CI 同款过滤表达式风格）。

- `cargo run -p easytier --bin easytier-core -- --help`  
  本地运行核心守护进程入口查看参数。

- `cargo run -p easytier --bin easytier-cli -- --help`  
  本地运行 CLI 入口查看子命令（peer/route/node/vpn-portal 等）。

- `cargo run -p easytier-web -- --help`  
  启动/调试 Web 配置服务入口（REST API + 控制台承载）。

## 高层架构（Big Picture）

### 1) 仓库形态：Rust + pnpm 双工作区

该仓库是一个跨平台网络系统 monorepo：

- Rust workspace（`Cargo.toml`）承载核心网络栈、Web 服务、GUI 后端、RPC 代码生成与各类适配层。
- pnpm workspace（`pnpm-workspace.yaml`）承载 Vue 前端（Web 控制台、GUI 前端、共享组件库、Tauri 插件 JS 包）。

`easytier` 与 `easytier-web` 是默认 Rust 成员，说明日常开发主路径是“核心网络引擎 + Web 控制平面”。

### 2) 核心域：`easytier` crate

`easytier` 同时提供：

- 库：`lib.rs`（被 GUI、Web、FFI/JNI 复用）
- 二进制：`easytier-core`（守护进程）与 `easytier-cli`（运维/查询）

可把它理解为三层：

1. 传输与连接层：`tunnel` + `connector`  
   `tunnel` 抽象统一了 TCP/UDP/WS/WG/QUIC/faketcp 等传输；`connector` 负责按 URL 与场景组装连接器，并包含 NAT 打洞逻辑（udp/tcp hole punch）。

2. 覆盖网络与路由层：`peers` / `peer_center` / `gateway` / `vpn_portal`  
   负责节点发现、邻接关系、路径选择、子网代理、VPN portal 等能力。README 中的“去中心化组网、共享节点、子网代理、WireGuard 门户”都落在这层与其编排逻辑。

3. 生命周期与服务编排层：`core` / `launcher` / `instance_manager` / `service_manager`  
   把配置、实例状态、后台服务、RPC 服务、系统集成串起来，形成可长期运行的节点进程。

### 3) 控制面与协议：Proto + 自定义 RPC 生成

RPC 不是“手写请求路由”，而是标准化生成链：

1. 协议定义在 `easytier/src/proto/*.proto`（如实例管理、节点管理、peer RPC、web heartbeat）。
2. `easytier/build.rs` 调用 `prost_build` 并接入 `easytier-rpc-build::ServiceGenerator`。
3. `easytier-rpc-build` 生成 trait/server/client/method descriptor 等胶水代码。
4. `easytier/src/proto/*.rs` 通过 `OUT_DIR` include 编译产物，`rpc_impl` 提供运行时分发与注册。
5. `rpc_service` 在核心启动流程中挂载服务实现。

这条链路是“控制面 API 演进”的关键：改 `.proto` 往往会影响 core、web、gui 三端。

### 4) Web 子系统：`easytier-web`

`easytier-web` 是“配置服务 + REST API + Web UI 承载层”，依赖 `easytier` crate：

- 后端：Axum 路由、会话、鉴权、存储与客户端会话管理（`client_manager`、`restful`）。
- 前端：`easytier-web/frontend`（Vue）通过 `/api/v1` 调后端。
- 可选嵌入：`embed` feature 将前端静态资源打包进 Rust 二进制（CI 会构建 `easytier-web --features=embed`）。

因此它既能独立提供管理面，也能与核心节点通过 RPC/心跳形成一体化控制流程。

### 5) GUI 子系统：`easytier-gui`（Tauri）

GUI 分前后端：

- 前端：Vue + `easytier-frontend-lib`。
- Tauri Rust 后端：直接依赖 `easytier` crate，通过 `invoke` 暴露命令给前端。

这意味着 GUI 不是简单外壳，而是“本地控制平面客户端 + core 调用桥”，在本地管理场景下可直接驱动核心能力。

### 6) 共享前端层：`easytier-web/frontend-lib`

`frontend-lib` 是 GUI 与 Web 控制台共享的组件/API 适配层，减少双端重复实现。看到 API 或 UI 逻辑同时影响 Web 与 GUI 时，优先检查该库。

### 7) 平台适配与扩展

`easytier-contrib` 与 `tauri-plugin-vpnservice` 承担生态扩展：

- `easytier-ffi`：C ABI 暴露。
- `easytier-android-jni`：Android JNI 接口。
- `easytier-uptime`：可用性/监控相关工具。
- `tauri-plugin-vpnservice`：Tauri 侧 VPN service 能力桥接。

这些项目通常复用 core，而不复制核心网络逻辑。

### 8) 运行模型要点（来自 README）

- 网络是去中心化的，节点地位对等；共享节点主要用于发现/中继兜底。
- 共享节点模式依赖统一 `--network-name` 与 `--network-secret`。
- 默认监听端口包含 TCP/UDP/WS/WSS/WireGuard（11010~11013）。
- 支持子网代理与 WireGuard portal，体现其“覆盖网络 + 接入网关”双重定位。

### 9) 本仓库的 AI 规则文件情况

未发现 `AGENTS.md`、`CLAUDE.md`、`.cursorrules` 或 `.github/copilot-instructions.md`。当前可依赖的规范来源主要是 `README.md`、`CONTRIBUTING.md` 与 CI workflows。