# Maylin 测试说明
- 测试完成：是（2026-10-04）
- 测试日期：2026-10-04
- 测试内容：单元 28（config/state/plugin/api/balancer 主路径+边界+错误路径）；集成 4（tests/auth_injection.rs，独立端口走真实 HTTP 节点）；注入 4（SQLi/XSS token→401、命令参数注入因 Command::new 无 shell 保持字面单参数、路径穿越→404）；钩子 2（插件未注册被拒、auth.fail 事件入环缓冲）。
- 运行命令：cargo test --bins（单元）；cargo test --test auth_injection（集成）；cargo test --test e2e（既有 E2E，需 node/python）
- 测试框架：Rust #[cfg(test)] + tests/ 集成测试
- 模型：豆包（Doubao）生成

本文件说明本仓库测试的范围、运行方式与预期结果。

## 测试目录约定

- **单元测试**：按 Rust 惯例，写在各 `src/*.rs` 内的 `#[cfg(test)] mod tests`，直接访问私有项。
- **集成测试**：放仓库根 `tests/` 目录。本 crate 是纯二进制（无 lib target），因此集成测试通过
  `CARGO_BIN_EXE_maylin` 拉起真实节点二进制、走真实 HTTP 验证行为。

## 运行命令（与 `.github/workflows/ci.yml` 一致）

```powershell
# 单元测试（CI: cargo test --bins）
cargo test --bins

# 已有 E2E（需要 node + python 运行时；CI: cargo test --test e2e -- --nocapture）
cargo test --test e2e -- --nocapture

# 新增：认证 / 注入 / RBAC / 事件钩子集成测试
cargo test --test auth_injection -- --nocapture

# 全量
cargo test
```

## 覆盖清单

### 单元测试（src 内，共 28 个）

| 模块 | 覆盖点 |
|---|---|
| `config.rs` | 最小配置默认值（data_dir/log_buffer_lines/alert/cluster）、`RestartPolicy` 解析与别名、`UserEntry` 默认 viewer、`LbCfg` 默认健康路径/间隔、`resolve_path` 绝对/相对、空目录加载、坏 TOML 报错、`gen_token` 长度与随机性、`bootstrap_if_missing` 首次生成+二次幂等、实例加载排序 |
| `state.rs` | `Role::parse/rank/as_str`（未知角色回退、越权写 root 不变 admin）、`RunStatus::alive/as_str`、`AppState` 角色映射（tokens→admin、users 按 role、空 token 跳过、非法 role→viewer）、日志文件路径 |
| `plugin.rs` | 占位符替换 `{script}/{port}/{name}`、启动方案合并、未知插件拒绝、空 command 拒绝、实例参数追加顺序、health_url 覆盖、**注入载荷作为字面参数不被 shell 拆分** |
| `api.rs` | RBAC 矩阵：GET 只读对 viewer 开放（终端除外）、start/stop/restart/reload 需 operator、exec/创建/删除/cluster 需 admin、DELETE 一律 admin、畸形路径不提升权限 |
| `balancer.rs` | 逐跳头（Host/Connection/Upgrade/Transfer-Encoding…）大小写不敏感过滤、普通内容头透传 |

### 集成测试（`tests/auth_injection.rs`，共 4 个）

拉起真实节点（独立端口，无实例/无 LB），通过 HTTP：

1. `auth_rejects_missing_wrong_and_injection_tokens` — 缺失/错误 token → 401；SQL 注入
   （`' OR '1'='1`、`" OR 1=1 --`、`'; DROP TABLE instances;--`）与 XSS
   （`<script>…</script>`、`{{7*7}}`）令牌一律 401；`?token=` 查询参数注入也被拒；正确 admin → 200。
2. `rbac_viewer_cannot_reach_sensitive_actions` — viewer 可读 `/api/status`，但 `POST /api/exec`、
   `POST /api/instances` → 403；admin 可正常 exec。
3. `path_traversal_does_not_leak_files` — `../`、URL 编码穿越访问实例名 → 404，响应体不含系统文件内容。
4. `auth_failures_are_recorded_as_events` — 失败认证触发 `auth.fail` 事件（事件钩子），viewer 可读事件流。

## 注入测试（输入被拒绝/转义，不透传）

- **命令注入**：`plugin.rs` 用 `Command::new(program).args(args)`（无 shell）。注入测试断言恶意
  name/script（含 `; && $(…) |`）作为**单个字面 argv 元素**传递，不被拆分执行。
- **认证注入**：SQL/XSS 令牌、`?token=` 注入均被当作未知凭证 → 401，不能绕过。
- **路径穿越**：`/api/instances/../..` 与编码变体 → 404，不泄露文件。

## 钩子测试（插件 / 事件机制）

- **插件注册**：`resolve()` 对未注册插件名返回错误（越权/未注册钩子被拒绝）；参数按模板正确传递；
  health_url 实例覆盖插件默认。
- **事件钩子**：认证失败 → `auth.fail` 事件入环缓冲并可经 `/api/events` 查询；事件 JSON 序列化输出。

## 预期结果

- `cargo test --bins`：**28 passed, 0 failed**。
- `cargo test --test auth_injection`：**4 passed, 0 failed**（每个测试独立端口，可并行）。
- `cargo test --test e2e`：仓库原有 E2E，需 node/python 运行时，保持既有行为不变。
