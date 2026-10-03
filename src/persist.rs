//! 运行状态持久化：定期把实例快照原子写入 data/state.json
//!
//! 用途：审计、外部监控读取、节点重启后追溯（重启计数/最后退出原因）。
//! 写入采用 tmp + rename 原子替换，避免读到半截文件。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::state::{AppState, RunStatus};

/// 后台每 10s 落一次快照
pub async fn spawn_save_loop(st: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            save(&st).await;
        }
    });
}

pub async fn save(st: &AppState) {
    let mut instances = Vec::new();
    for name in st.instance_names().await {
        if let Some(i) = st.get_instance(&name).await {
            let status = i.status().await;
            instances.push(json!({
                "name": name,
                "status": status.as_str(),
                "pid": i.pid.load(std::sync::atomic::Ordering::SeqCst),
                "restarts": i.restarts.load(std::sync::atomic::Ordering::SeqCst),
                "started_at": i.started_at.read().await.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
                "last_exit": i.last_exit.read().await.clone(),
            }));
        }
    }
    let doc = json!({
        "node": st.cfg.node.name,
        "version": env!("CARGO_PKG_VERSION"),
        "saved_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "instances": instances,
    });
    write_atomic(&st.data_dir.join("state.json"), &doc).await;
}

/// tmp + rename 原子写入（在阻塞线程执行，避免卡 async）
async fn write_atomic(path: &std::path::Path, v: &Value) {
    let body = match serde_json::to_string_pretty(v) {
        Ok(s) => s,
        Err(_) => return,
    };
    let tmp = path.with_extension("json.tmp");
    let p = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    })
    .await;
}

// RunStatus 在此处仅为类型完整性引用（快照通过 as_str 输出）
#[allow(dead_code)]
fn _touch(_: RunStatus) {}
