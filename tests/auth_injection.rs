//! 认证 / 注入 / RBAC / 事件钩子 集成测试
//!
//! 拉起真实 maylin 二进制（CARGO_BIN_EXE_maylin），用最小配置（无实例、无负载均衡）
//! 监听独立端口，通过真实 HTTP 请求验证：
//!   * 缺失 / 错误 / SQL 注入 / XSS 令牌一律 401（认证不被绕过）
//!   * viewer 角色不能执行 admin/operator 敏感操作（403）
//!   * 路径穿越不会泄露文件或越权（404）
//!   * 认证失败会写入事件流（事件钩子 auth.fail 触发）
//!
//! 运行：cargo test --test auth_injection -- --nocapture
//! （每个测试使用独立端口，可并行运行）

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;

const ADMIN: &str = "adm-secret-1234567890";
const VIEWER: &str = "vie-secret-abcdefghij";

struct NodeProc {
    child: std::process::Child,
}

impl Drop for NodeProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_file(path: &Path, body: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn setup_dir(port: u16) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("maylin-auth-it-{port}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_file(
        &dir.join("maylin.toml"),
        &format!(
            r#"[node]
name = "auth-it-{port}"
listen = "127.0.0.1:{port}"
data_dir = "data"

[auth]
tokens = ["{ADMIN}"]
[[auth.users]]
name = "read-only"
token = "{VIEWER}"
role = "viewer"

[alert]
enabled = false

[cluster]
enabled = false
"#
        ),
    );
    dir
}

fn spawn_node(dir: &Path) -> NodeProc {
    let cfg = dir.join("maylin.toml");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_maylin"));
    cmd.args(["node", "--config"])
        .arg(&cfg)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    NodeProc {
        child: cmd.spawn().expect("启动 maylin node 失败"),
    }
}

fn base(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// 等待 API 就绪：收到任意 HTTP 响应（含 401）即可
async fn wait_api_up(client: &Client, port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if client
            .get(format!("{}/api/status", base(port)))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    panic!("maylin API(port {port}) 未在超时内就绪");
}

#[tokio::test]
async fn auth_rejects_missing_wrong_and_injection_tokens() {
    let port = 17411;
    let dir = setup_dir(port);
    let _node = spawn_node(&dir);
    let client = Client::new();
    wait_api_up(&client, port, Duration::from_secs(15)).await;

    // 1) 完全不带 token -> 401
    let st = client
        .get(format!("{}/api/status", base(port)))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, reqwest::StatusCode::UNAUTHORIZED, "缺失 token 必须 401");

    // 2) 错误 token -> 401
    let st = client
        .get(format!("{}/api/status", base(port)))
        .bearer_auth("definitely-wrong")
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, reqwest::StatusCode::UNAUTHORIZED);

    // 3) SQL 注入令牌 -> 仍 401（不能拼出真值）
    for evil in [
        "' OR '1'='1",
        "\" OR 1=1 --",
        "admin' --",
        "'; DROP TABLE instances;--",
    ] {
        let st = client
            .get(format!("{}/api/status", base(port)))
            .bearer_auth(evil)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(st, reqwest::StatusCode::UNAUTHORIZED, "SQL 注入 token {evil:?} 必须被拒");
    }

    // 4) XSS 令牌 -> 401，且不会被当成合法凭证
    for evil in ["<script>alert(1)</script>", "<img src=x onerror=alert(1)>", "{{7*7}}"] {
        let st = client
            .get(format!("{}/api/status", base(port)))
            .bearer_auth(evil)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(st, reqwest::StatusCode::UNAUTHORIZED, "XSS token {evil:?} 必须被拒");
    }

    // 5) 走 ?token= 查询参数的注入也被拒
    let st = client
        .get(format!("{}/api/status?token='%20OR%201=1--", base(port)))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, reqwest::StatusCode::UNAUTHORIZED);

    // 6) 正确 admin token -> 200
    let st = client
        .get(format!("{}/api/status", base(port)))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, reqwest::StatusCode::OK);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rbac_viewer_cannot_reach_sensitive_actions() {
    let port = 17412;
    let dir = setup_dir(port);
    let _node = spawn_node(&dir);
    let client = Client::new();
    wait_api_up(&client, port, Duration::from_secs(15)).await;

    // viewer 只读 GET -> 200
    let st = client
        .get(format!("{}/api/status", base(port)))
        .bearer_auth(VIEWER)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, reqwest::StatusCode::OK, "viewer 应可读 /api/status");

    // viewer POST /api/exec（admin-only shell）-> 403
    let st = client
        .post(format!("{}/api/exec", base(port)))
        .bearer_auth(VIEWER)
        .json(&serde_json::json!({"command": "echo pwned"}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, reqwest::StatusCode::FORBIDDEN, "viewer 不能调用 exec");

    // viewer POST 创建实例 -> admin-only -> 403
    let st = client
        .post(format!("{}/api/instances", base(port)))
        .bearer_auth(VIEWER)
        .json(&serde_json::json!({"name":"x","plugin":"node"}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, reqwest::StatusCode::FORBIDDEN);

    // admin 调用 echo 是允许的（不是 401/403）
    let st = client
        .post(format!("{}/api/exec", base(port)))
        .bearer_auth(ADMIN)
        .json(&serde_json::json!({"command": "echo hello"}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, reqwest::StatusCode::OK);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn path_traversal_does_not_leak_files() {
    let port = 17413;
    let dir = setup_dir(port);
    let _node = spawn_node(&dir);
    let client = Client::new();
    wait_api_up(&client, port, Duration::from_secs(15)).await;

    // 路径穿越访问实例名：必须 404（实例不存在），绝不能 200 或回退到文件系统
    for evil in [
        "/api/instances/../../../../etc/passwd",
        "/api/instances/..%2F..%2F..%2Fetc%2Fpasswd",
        "/api/instances/%2e%2e/%2e%2e/win.ini",
    ] {
        let resp = client
            .get(format!("{}{}", base(port), evil))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        let st = resp.status();
        assert!(
            st == reqwest::StatusCode::NOT_FOUND,
            "路径穿越 {evil} 应 404，实际 {st}"
        );
        let body = resp.text().await.unwrap_or_default();
        assert!(
            !body.contains("[fonts]") && !body.contains("root:"),
            "不应泄露系统文件内容: {body}"
        );
    }
}

#[tokio::test]
async fn auth_failures_are_recorded_as_events() {
    let port = 17414;
    let dir = setup_dir(port);
    let _node = spawn_node(&dir);
    let client = Client::new();
    wait_api_up(&client, port, Duration::from_secs(15)).await;

    // 制造若干次失败认证（auth.fail 事件）
    for _ in 0..3 {
        let _ = client
            .get(format!("{}/api/status", base(port)))
            .bearer_auth("bad-token-for-event")
            .send()
            .await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // admin 查询事件流（事件钩子：认证失败 -> auth.fail 事件）
    let resp: Value = client
        .get(format!("{}/api/events?limit=50", base(port)))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = resp["events"].as_array().unwrap();
    let has_auth_fail = items.iter().any(|e| e["kind"] == "auth.fail");
    assert!(has_auth_fail, "失败认证应产生 auth.fail 事件，实际: {items:?}");

    // viewer 也能读事件（GET 开放）
    let viewer_sees = client
        .get(format!("{}/api/events?limit=50", base(port)))
        .bearer_auth(VIEWER)
        .send()
        .await
        .unwrap();
    assert_eq!(viewer_sees.status(), reqwest::StatusCode::OK);

    let _ = std::fs::remove_dir_all(&dir);
}
