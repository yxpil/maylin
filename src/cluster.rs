use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use crate::state::AppState;

fn peer_token(st: &Arc<AppState>) -> String {
    st.cfg
        .cluster
        .token
        .clone()
        .unwrap_or_else(|| st.token().to_string())
}

async fn peer_get(st: &Arc<AppState>, peer: &str, path: &str) -> Result<Value> {
    let url = format!("{}{}", peer.trim_end_matches('/'), path);
    let resp = st
        .local_client
        .get(&url)
        .timeout(Duration::from_secs(5))
        .bearer_auth(peer_token(st))
        .send()
        .await?;
    let v: Value = resp.error_for_status()?.json().await?;
    Ok(v)
}

async fn peer_post(st: &Arc<AppState>, peer: &str, path: &str) -> Result<Value> {
    let url = format!("{}{}", peer.trim_end_matches('/'), path);
    let resp = st
        .local_client
        .post(&url)
        .timeout(Duration::from_secs(30))
        .bearer_auth(peer_token(st))
        .send()
        .await?;
    let v: Value = resp.error_for_status()?.json().await?;
    Ok(v)
}

/// 聚合本节点与所有 peer 的状态
pub async fn cluster_status(st: &Arc<AppState>) -> Value {
    let mut nodes = Vec::new();
    // 本节点
    nodes.push(json!({
        "peer": "self",
        "name": st.cfg.node.name,
        "ok": true,
    }));
    for peer in &st.cfg.cluster.peers {
        let r = peer_get(st, peer, "/api/status").await;
        match r {
            Ok(v) => nodes.push(json!({ "peer": peer, "ok": true, "status": v })),
            Err(e) => nodes.push(json!({ "peer": peer, "ok": false, "error": e.to_string() })),
        }
    }
    json!({ "cluster": st.cfg.node.name, "peers_enabled": st.cfg.cluster.enabled, "nodes": nodes })
}

/// 向所有 peer 广播动作（start/stop/restart），target 可为实例名或 "all"
pub async fn fanout_action(st: &Arc<AppState>, action: &str, target: &str) -> Value {
    let mut results = Vec::new();
    for peer in &st.cfg.cluster.peers {
        let r = if target == "all" {
            // 取 peer 实例列表后逐个执行
            let mut sub = Vec::new();
            match peer_get(st, peer, "/api/instances").await {
                Ok(v) => {
                    if let Some(list) = v.as_array() {
                        for it in list {
                            if let Some(name) = it.get("name").and_then(|x| x.as_str()) {
                                let path = format!("/api/instances/{name}/{action}");
                                sub.push(peer_post(st, peer, &path).await);
                            }
                        }
                    }
                    let arr: Vec<Value> = sub
                        .into_iter()
                        .map(|r| match r {
                            Ok(v) => v,
                            Err(e) => json!({ "error": e.to_string() }),
                        })
                        .collect();
                    Ok(Value::Array(arr))
                }
                Err(e) => Err(e),
            }
        } else {
            peer_post(st, peer, &format!("/api/instances/{target}/{action}")).await
        };
        match r {
            Ok(v) => results.push(json!({ "peer": peer, "ok": true, "result": v })),
            Err(e) => results.push(json!({ "peer": peer, "ok": false, "error": e.to_string() })),
        }
    }
    json!({ "action": action, "target": target, "results": results })
}
