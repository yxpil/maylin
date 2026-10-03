# Maylin — 无 Docker 的 Rust 服务集群管理器

[![CI](https://github.com/yxpil/maylin/actions/workflows/ci.yml/badge.svg)](https://github.com/yxpil/maylin/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/yxpil/maylin?color=4ea1ff)](https://github.com/yxpil/maylin/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-22d3a7)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-windows%20%7C%20linux%20%7C%20macos-8b9bb0)](#跨平台)

> 📖 项目主页（含架构图与实测数据）：**https://yxpil.github.io/maylin/**

通过 **Rust 子进程**直接管理任意数量的 JS / Python / 二进制实例（每个实例独占端口，无需 Docker），
以**插件**适配运行时，支持**多服务器集群**、**Token 控制 API**、**命令终端**、**定时启动**与**负载均衡**。

```
maylinctl / HTTP API / WS 终端
        │  Bearer Token
        ▼
┌─────────────────────────── maylind 节点 ───────────────────────────┐
│  进程管理器(子进程 supervisor)   插件(node/python/binary/自定义)    │
│  定时任务(cron)                 负载均衡(反向代理, 跨节点上游)      │
│  REST API + WebSocket 终端      集群 peers 聚合 / 动作广播          │
└───────────────────────────────────────────────────────────────────┘
   instance:demo-node:3001   instance:demo-python:3002   ...
```

## 安装

三种方式任选：

```bash
# 方式一：下载预编译二进制（Windows / Linux / macOS，见 Releases）
# https://github.com/yxpil/maylin/releases

# 方式二：从源码构建
git clone https://github.com/yxpil/maylin && cd maylin
cargo build --release

# 方式三：cargo install（从 git）
cargo install --git https://github.com/yxpil/maylin
```

## 快速开始

```bash
# 1. 启动（首次运行自动生成 config/ 默认配置 + 随机 Token + demo 示例）
target/release/maylin node --config config/maylin.toml

# 若想手动放置配置：cp -r config.example config 后修改即可（示例模板不含真实 Token）

# 2. 管理客户端
export MAYLIN_TOKEN=<配置中的 tokens[0]>
export MAYLIN_URL=http://127.0.0.1:7000

maylin ctl instances              # 实例列表
maylin ctl start demo-node        # 启动 Node 实例 (:3001)
maylin ctl start demo-python      # 启动 Python 实例 (:3002)
maylin ctl status                 # 节点状态
maylin ctl logs demo-node -f      # 跟踪日志
maylin ctl terminal demo-node     # 交互式终端（stdin/stdout 接入）
maylin ctl exec "dir"             # 在节点上执行命令
maylin ctl events                 # 事件审计
maylin ctl reload                 # 重新扫描配置目录

# 3. 负载均衡（配置中已带 :8000 -> 3001/3002 的示例）
curl http://127.0.0.1:8000/       # 轮询命中两个实例
```

## CI 与测试

仓库配置了三套 GitHub Actions：

| 工作流 | 触发 | 做什么 |
|---|---|---|
| **CI**（`.github/workflows/ci.yml`） | push / PR | Windows + Linux 双平台 `cargo build --release`、`cargo test --test e2e`、fmt 检查与 clippy |
| **Release**（`.github/workflows/release.yml`） | 推送 `v*` 标签 | 三平台构建二进制并发布 Release 附件（含 SHA256SUMS） |
| **Pages**（`.github/workflows/pages.yml`） | `docs/**` 变更 | 部署项目展示页到 GitHub Pages |

### E2E 测试

集成测试会拉起真实节点二进制（独立端口，不与开发环境冲突），
完整验证：实例启停 → 直连/LB 轮询 → 故障转移 → exec → 认证拒绝 → 事件 → 状态持久化：

```bash
cargo test --test e2e -- --nocapture   # 约 20s，需要 PATH 中有 node 与 python
```

## 目录结构

```
config.example/           # 配置模板（不含真实 Token，可 cp -r 后使用）
config/maylin.toml        # 主配置：节点名/监听/Token/角色/告警/集群peers/负载均衡
config/plugins/*.toml     # 运行时插件（如何启动某一类程序）
config/instances/*.toml   # 实例定义（每个实例 = 一个子进程 + 独占端口）
config/schedules/*.toml   # 定时任务（cron 启动/停止/重启）
demo/                     # 示例 JS / Python 应用
docs/index.html           # GitHub Pages 项目主页
data/logs/<name>.log      # 实例运行日志（持久化）
data/events.jsonl         # 事件审计日志（追加式，持久化）
data/state.json           # 运行状态快照（每 10s 原子写入）
```

## 插件系统

一个插件 = 一类运行时的启动模板，支持 `{script}` `{port}` `{name}` 占位符：

```toml
# config/plugins/node.toml
[plugin]
name = "node"
command = "node"
args = ["{script}"]
health_url = "http://127.0.0.1:{port}/health"   # 可选：健康检查模板
health_interval_secs = 10
grace_secs = 5                                   # 停止宽限，超时强杀
shutdown_line = "exit"                           # 可选：优雅停止时写入 stdin 的行
```

新增一种运行时（如 java、go 二进制）只需加一个 toml 文件，无需改代码。

## 实例定义

```toml
# config/instances/demo-node.toml
[instance]
name = "demo-node"
plugin = "node"
workdir = "demo"
script = "server.js"
args = ["--port", "{port}"]
port = 3001                       # 注入 PORT 环境变量 + 占位符替换
autostart = true                  # 节点启动时自动拉起
restart_policy = "on-failure"     # no | on-failure | always
max_retries = 5                   # 连续重启上限（稳定运行2分钟后重置）
health_url = "http://127.0.0.1:{port}/health"
```

## 多服务器集群

每台服务器运行一个 `maylin node`，主配置中互相配置 peers：

```toml
[cluster]
enabled = true
peers = ["http://192.168.1.10:7000", "http://192.168.1.11:7000"]
token = "集群互访令牌（需同时存在于各节点 auth.tokens）"
```

- `maylin ctl cluster` — 聚合所有节点状态
- `maylin ctl cluster-action restart all` — 向所有节点广播动作
- 负载均衡 upstreams 可直接填远端节点端口，实现跨服务器流量分发
- 任何节点均可被 ctl 直连管理：`maylin ctl --url http://192.168.1.11:7000 --token xxx instances`

## 定时任务

```toml
# config/schedules/nightly-restart.toml
[schedule]
name = "nightly-restart"
cron = "0 3 * * *"        # 分 时 日 月 周（标准 5 段）
action = "restart"        # start | stop | restart
target = "all"            # 实例名或 "all"
enabled = true
```

## HTTP API（均需 `Authorization: Bearer <token>`，WS 可用 `?token=`；受 RBAC 角色限制）

| 方法 | 路径 | 最低角色 | 说明 |
|---|---|---|---|
| GET | `/api/status` | viewer | 节点状态（实例+LB+指标） |
| GET/POST | `/api/instances` | viewer/admin | 实例列表 / 创建实例（JSON） |
| GET/DELETE | `/api/instances/{name}` | viewer/admin | 实例详情 / 删除实例 |
| POST | `/api/instances/{name}/start\|stop\|restart` | operator | 生命周期控制 |
| GET | `/api/instances/{name}/logs?lines=100` | viewer | 日志（JSON 行） |
| GET(ws) | `/api/instances/{name}/terminal` | operator | WebSocket 终端（stdin/stdout） |
| POST | `/api/exec` | admin | `{command, timeout_secs}` 执行 shell 命令 |
| GET | `/api/plugins` / `/api/schedules` | viewer | 插件与定时任务 |
| POST | `/api/reload` | operator | 重新扫描配置目录 |
| GET | `/api/lb/status` | viewer | 负载均衡上游健康状态 |
| GET | `/api/cluster/status` | viewer | 集群聚合状态 |
| POST | `/api/cluster/action` | admin | `{action,target}` 广播动作 |
| GET | `/api/events?limit=100` | viewer | 最近事件（告警/审计） |

## 生产加固（v0.2）

### RBAC 权限

`auth.tokens` 中的 token = **admin**（全部权限）。还可定义细粒度角色：

```toml
[[auth.users]]
name = "ops"
token = "另一个随机长串"
role = "operator"    # admin | operator | viewer
```

| 角色 | 权限 |
|---|---|
| `admin` | 全部（创建/删除实例、exec、集群广播） |
| `operator` | start/stop/restart、reload、终端（可写 stdin） |
| `viewer` | 只读（status/instances/logs/lb/events 等 GET） |

权限不足返回 `403`，认证失败返回 `401`（限流记录 `auth.fail` 事件）。

### 事件与告警

所有运行事件写入内存环（500 条）+ `data/events.jsonl`（永久落盘），
支持 `ctl events` 查看，并可推送 webhook：

```toml
[alert]
enabled = true
webhook_url = "https://example.com/hook"   # 事件触发时 POST JSON
events = ["instance.crash", "instance.fail", "instance.giveup", "lb.down", "auth.fail"]
timeout_secs = 5
```

事件类型：`node.start/stop`、`instance.start/stop/exit/restart/giveup/fail/unhealthy`、
`lb.down/up`、`schedule.fire`、`exec.run`、`auth.fail`（每分钟限 1 条防刷）。

### 持久化

- `data/events.jsonl` — 事件审计日志（追加式 JSONL）
- `data/state.json` — 每 10 秒原子快照（tmp+rename）：各实例状态/重启计数/最后退出原因，供外部监控读取
- `data/logs/<name>.log` — 实例运行日志

### 容错

- 崩溃自动重启：指数退避（2s×n，上限 30s），`max_retries` 上限后熔断并发出 `instance.giveup` 告警
- exec 并发信号量（≤4），防止 fork 风暴
- 优雅退出：Ctrl-C / SIGTERM（unix）→ 停全部实例 → 落盘快照 → 退出
- LB 上游故障自动摘除 + 请求级失败重试下一个上游（故障转移零失败）

### 性能

- HTTP 客户端连接池调优：`pool_max_idle_per_host=64`、`tcp_nodelay`、`tcp_keepalive`
- release：`lto=thin + codegen-units=1`
- 内置 `/api/status` 指标（api/lb/exec/auth_fails 计数）与 LB 每上游健康/请求数

### 高并发压测

```bash
cargo build --release --example loadtest
# LB 压测：并发 100 × 5000 请求
target/release/examples/loadtest.exe http://127.0.0.1:8000/ 100 5000
# API 压测（带 Token）
target/release/examples/loadtest.exe http://127.0.0.1:7000/api/status 50 2000 <token>
```

输出 RPS 与 p50/p90/p95/p99/max 延迟。
实测参考（本机 Windows）：LB 转发 ~10,900 RPS（p99 47ms）；API ~61,000 RPS（p99 5ms）。

### 跨平台

- Windows：经典 `CreatePipe` + `PeekNamedPipe` 轮询读（规避本机命名管道限制），`CREATE_NO_WINDOW` 防弹窗
- Linux/macOS：`std::io::pipe` + BufRead 逐行读，SIGTERM 优雅退出
- 无平台专属 API 泄漏到主流程，`cargo build` 在两端可直接编译

## 设计要点

- **无 Docker**：直接管理子进程；Windows 上使用经典 `CreatePipe` 匿名管道 + `CREATE_NO_WINDOW` + 手动生命周期管理（不依赖 tokio 的命名管道实现，规避部分环境下 `os error 231` 管道创建失败的问题）
- **日志不依赖 EOF**：输出泵通过 `PeekNamedPipe` 轮询 + 子进程退出标志排空数据（部分安全软件会额外持有继承句柄导致 EOF 永不到达）
- **内部流量不走系统代理**：健康检查 / 负载均衡转发 / 集群通信强制直连（`no_proxy`），避免 HTTP_PROXY 环境变量劫持本机回环流量
- **supervisor 守护**：崩溃按 restart_policy 自动拉起，指数退避（2s×n，上限 30s）
- **优雅停止**：先写 `shutdown_line` 到 stdin，宽限 `grace_secs` 后强杀
- **健康检查**：实例级 HTTP 探活，连续 3 次失败标记 unhealthy；LB 上游独立探活，故障自动摘除；转发请求失败自动重试下一个上游，消除摘除窗口期
- **Token 安全**：所有 API（含 WebSocket）强制 Bearer Token

## Windows 注意

`maylin ctl exec` 在 Windows 走 `cmd /C`，在 Linux/macOS 走 `bash -lc`。
端口占用请先停止占用进程；插件 `command` 需在 PATH 中或使用绝对路径。
若守护进程被强杀（非 Ctrl-C），其子进程可能成为孤儿进程，请手动清理。
