//! 事件系统：内存环缓冲 + JSONL 落盘 + Webhook 告警
//!
//! 所有运行事件（实例启停/崩溃/重启、LB 上下游、定时任务、认证失败等）
//! 统一经由 `emit` 记录，供审计、告警与 `ctl events` 查询。

use std::sync::atomic::{AtomicI64, Ordering};

use serde::Serialize;
use serde_json::json;

use crate::state::AppState;

const RING_CAP: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub ts: String,
    /// instance.start / instance.exit / instance.restart / instance.giveup /
    /// instance.fail / instance.unhealthy / lb.down / lb.up / schedule.fire / auth.fail / node.start
    pub kind: String,
    pub target: String,
    /// info | warn | error
    pub level: String,
    pub message: String,
}

/// auth.fail 事件限流：同一分钟最多 1 条，防止爆破刷盘
static LAST_AUTH_FAIL: AtomicI64 = AtomicI64::new(0);

/// 记录事件：入环缓冲 → 异步落盘 →（可选）webhook 告警
pub async fn emit(st: &AppState, kind: &str, target: &str, level: &str, message: &str) {
    let ev = Event {
        ts: chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S%.3f")
            .to_string(),
        kind: kind.into(),
        target: target.into(),
        level: level.into(),
        message: message.into(),
    };

    {
        let mut g = st.events.write().await;
        g.push_back(ev.clone());
        while g.len() > RING_CAP {
            g.pop_front();
        }
    }

    // 落盘（异步，失败不影响主流程）
    let path = st.data_dir.join("events.jsonl");
    let line = match serde_json::to_string(&ev) {
        Ok(s) => s,
        Err(_) => return,
    };
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        if let Ok(mut f) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            let _ = f.write_all(format!("{line}\n").as_bytes()).await;
        }
    });

    // webhook 告警
    let cfg = &st.cfg.alert;
    if !cfg.enabled || cfg.webhook_url.is_empty() {
        return;
    }
    if !cfg.events.is_empty() && !cfg.events.iter().any(|k| k == kind) {
        return;
    }
    let url = cfg.webhook_url.clone();
    let timeout = std::time::Duration::from_secs(cfg.timeout_secs.max(1));
    let client = st.client.clone();
    let payload = json!({
        "node": st.cfg.node.name,
        "event": ev,
    });
    tokio::spawn(async move {
        let _ = client
            .post(&url)
            .json(&payload)
            .timeout(timeout)
            .send()
            .await;
    });
}

/// 认证失败事件（限流）
pub async fn emit_auth_fail(st: &AppState, detail: String) {
    let now = chrono::Utc::now().timestamp();
    let last = LAST_AUTH_FAIL.load(Ordering::SeqCst);
    if now - last < 60 {
        return;
    }
    if LAST_AUTH_FAIL
        .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    emit(st, "auth.fail", "-", "warn", &detail).await;
}
