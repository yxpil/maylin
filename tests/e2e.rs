//! Maylin E2E 集成测试
//!
//! 拉起真实节点二进制（CARGO_BIN_EXE_maylin），使用独立端口，
//! 完整走一遍：实例启停 → 直连/负载均衡 → 故障转移 → exec → RBAC → 事件 → 持久化。
//!
//! 运行：cargo test --test e2e -- --nocapture

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;

// 独立端口，避免与开发环境 (7000/8000/3001/3002) 冲突
const API_PORT: u16 = 17300;
const LB_PORT: u16 = 18000;
const INST1_PORT: u16 = 13301;
const INST2_PORT: u16 = 13302;

const TOKEN: &str = "e2e-token-0123456789abcdef";

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

fn setup_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("maylin-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let demo_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("demo");
    let demo_dst = dir.join("demo");
    std::fs::create_dir_all(&demo_dst).unwrap();
    for f in ["server.js", "app.py"] {
        std::fs::copy(demo_src.join(f), demo_dst.join(f)).unwrap();
    }

    let demo_abs = demo_dst.display().to_string().replace('\\', "/");

    write_file(
        &dir.join("maylin.toml"),
        &format!(
            r#"[node]
name = "e2e-node"
listen = "127.0.0.1:{API_PORT}"
data_dir = "data"
log_buffer_lines = 500

[auth]
tokens = ["{TOKEN}"]

[alert]
enabled = false

[cluster]
enabled = false

[[load_balancer]]
name = "web"
listen = "127.0.0.1:{LB_PORT}"
upstreams = [
    "http://127.0.0.1:{INST1_PORT}",
    "http://127.0.0.1:{INST2_PORT}",
]
health_path = "/health"
health_interval_secs = 3
"#
        ),
    );

    write_file(
        &dir.join("plugins/node.toml"),
        r#"[plugin]
name = "node"
command = "node"
args = ["{script}"]
health_interval_secs = 3
grace_secs = 3
"#,
    );
    write_file(
        &dir.join("plugins/python.toml"),
        r#"[plugin]
name = "python"
command = "python"
args = ["{script}"]
health_interval_secs = 3
grace_secs = 3
"#,
    );

    write_file(
        &dir.join("instances/demo-1.toml"),
        &format!(
            r#"[instance]
name = "demo-1"
plugin = "node"
workdir = "{demo_abs}"
script = "server.js"
args = ["--port", "{{port}}"]
port = {INST1_PORT}
autostart = false
restart_policy = "on-failure"
max_retries = 3
health_url = "http://127.0.0.1:{{port}}/health"
"#
        ),
    );
    write_file(
        &dir.join("instances/demo-2.toml"),
        &format!(
            r#"[instance]
name = "demo-2"
plugin = "python"
workdir = "{demo_abs}"
script = "app.py"
args = ["--port", "{{port}}"]
port = {INST2_PORT}
autostart = false
restart_policy = "on-failure"
max_retries = 3
health_url = "http://127.0.0.1:{{port}}/health"
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
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    NodeProc {
        child: cmd.spawn().expect("启动 maylin node 失败"),
    }
}

async fn wait_http_ok(client: &Client, url: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if client
            .get(url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    false
}

/// 等待 API 就绪（收到任意 HTTP 响应即认为节点已起，含 401）
async fn wait_api_up(client: &Client, url: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if client
            .get(url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    false
}

async fn api_get(client: &Client, path: &str) -> Value {
    client
        .get(format!("http://127.0.0.1:{API_PORT}{path}"))
        .timeout(Duration::from_secs(10))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .expect("请求失败")
        .error_for_status()
        .expect("API 返回错误")
        .json::<Value>()
        .await
        .expect("JSON 解析失败")
}

async fn inst_status(client: &Client, name: &str) -> String {
    let v = api_get(client, &format!("/api/instances/{name}")).await;
    v["status"].as_str().unwrap_or("?").to_string()
}

async fn wait_status(client: &Client, name: &str, want: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let s = inst_status(client, name).await;
        if s == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "实例 {name} 未在超时内进入 {want}（当前 {s}）"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn api_post(client: &Client, path: &str) -> u16 {
    client
        .post(format!("http://127.0.0.1:{API_PORT}{path}"))
        .timeout(Duration::from_secs(30))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .map(|r| r.status().as_u16())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_full_lifecycle() {
    let dir = setup_dir();
    let _node = spawn_node(&dir);
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let auth = format!("Bearer {TOKEN}");
    let base = format!("http://127.0.0.1:{API_PORT}");

    // 1. 节点就绪（收到任意响应即就绪；/api/status 需要 token，这里只探测连通性）
    assert!(
        wait_api_up(
            &client,
            &format!("{base}/api/status"),
            Duration::from_secs(30)
        )
        .await,
        "节点 30s 内未就绪"
    );
    let status = api_get(&client, "/api/status").await;
    assert_eq!(status["node"], "e2e-node");
    assert_eq!(status["instances"].as_array().unwrap().len(), 2);
    println!("[1] 节点就绪 OK");

    // 2. 启动两个实例并进入 running
    assert_eq!(api_post(&client, "/api/instances/demo-1/start").await, 200);
    assert_eq!(api_post(&client, "/api/instances/demo-2/start").await, 200);
    wait_status(&client, "demo-1", "running", Duration::from_secs(20)).await;
    wait_status(&client, "demo-2", "running", Duration::from_secs(20)).await;
    println!("[2] 双实例 running OK");

    // 3. 直连实例端口
    for p in [INST1_PORT, INST2_PORT] {
        assert!(
            wait_http_ok(
                &client,
                &format!("http://127.0.0.1:{p}/"),
                Duration::from_secs(10)
            )
            .await,
            "实例端口 {p} 不可达"
        );
    }
    println!("[3] 实例直连 OK");

    // 4. LB 轮询命中两个实例
    let mut hit_node = false;
    let mut hit_py = false;
    for _ in 0..6 {
        let body = client
            .get(format!("http://127.0.0.1:{LB_PORT}/"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        if body.contains("node-demo") {
            hit_node = true;
        }
        if body.contains("python-demo") {
            hit_py = true;
        }
    }
    assert!(
        hit_node && hit_py,
        "LB 轮询未覆盖双实例 (node={hit_node} py={hit_py})"
    );
    println!("[4] LB 轮询 OK");

    // 5. 故障转移：停掉 demo-1，LB 仍 200（重试切到 demo-2）
    assert_eq!(api_post(&client, "/api/instances/demo-1/stop").await, 200);
    wait_status(&client, "demo-1", "stopped", Duration::from_secs(15)).await;
    for _ in 0..3 {
        let r = client
            .get(format!("http://127.0.0.1:{LB_PORT}/"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "故障转移后 LB 返回 {}", r.status());
        assert!(r.text().await.unwrap().contains("python-demo"));
    }
    println!("[5] LB 故障转移 OK");

    // 6. 重启恢复 + 崩溃重启能力（重启后 running）
    assert_eq!(
        api_post(&client, "/api/instances/demo-1/restart").await,
        200
    );
    wait_status(&client, "demo-1", "running", Duration::from_secs(20)).await;
    println!("[6] 实例恢复 OK");

    // 7. exec（admin）：执行命令并返回输出
    let v = client
        .post(format!("{base}/api/exec"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({ "command": "echo e2e-exec-ok", "timeout_secs": 10 }))
        .send()
        .await
        .unwrap();
    assert_eq!(v.status().as_u16(), 200, "exec 应 200");
    let out: Value = v.json().await.unwrap();
    assert!(
        out["stdout"].as_str().unwrap_or("").contains("e2e-exec-ok"),
        "exec 输出异常: {out}"
    );
    println!("[7] exec OK");

    // 8. RBAC：无效 token 401
    let code = client
        .get(format!("{base}/api/status"))
        .header("Authorization", "Bearer wrong-token")
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(code, 401, "错误 token 应 401");
    println!("[8] 认证拒绝 OK");

    // 9. 事件系统：instance.start 已被记录
    let ev = api_get(&client, "/api/events?limit=100").await;
    let kinds: Vec<&str> = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"instance.start"),
        "缺少 instance.start 事件: {kinds:?}"
    );
    assert!(
        kinds.contains(&"instance.stop"),
        "缺少 instance.stop 事件: {kinds:?}"
    );
    println!("[9] 事件系统 OK");

    // 10. 持久化：state.json 快照存在且含实例
    let state_path = dir.join("data/state.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if state_path.exists() {
            break;
        }
        assert!(Instant::now() < deadline, "state.json 未生成");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state["node"], "e2e-node");
    assert!(state["instances"].as_array().unwrap().len() >= 2);
    println!("[10] 状态持久化 OK");

    // 11. 清理：停实例（避免孤儿进程），daemon 由 Drop 兜底 kill
    let _ = api_post(&client, "/api/instances/demo-1/stop").await;
    let _ = api_post(&client, "/api/instances/demo-2/stop").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let _ = std::fs::remove_dir_all(&dir);
    println!("[11] 清理完成 —— E2E 全部通过 ✔");
}
