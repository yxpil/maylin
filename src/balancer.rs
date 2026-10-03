use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::HeaderName;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct UpstreamStatus {
    pub url: String,
    pub healthy: bool,
    pub fails: u32,
}

pub struct LbRuntime {
    pub name: String,
    pub listen: String,
    pub health_path: String,
    pub ups: RwLock<Vec<UpstreamStatus>>,
    pub rr: AtomicUsize,
    pub request_count: AtomicUsize,
    pub client: reqwest::Client,
}

impl LbRuntime {
    pub async fn snapshot(&self) -> LbSnapshot {
        let ups = self.ups.read().await.clone();
        LbSnapshot {
            name: self.name.clone(),
            listen: self.listen.clone(),
            requests: self.request_count.load(Ordering::SeqCst),
            upstreams: ups,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct LbSnapshot {
    pub name: String,
    pub listen: String,
    pub requests: usize,
    pub upstreams: Vec<UpstreamStatus>,
}

const HOP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

fn is_hop(name: &HeaderName) -> bool {
    HOP_HEADERS.iter().any(|h| name.as_str().eq_ignore_ascii_case(h))
}

/// 为每个 LB 配置启动反向代理服务 + 健康检查循环
pub async fn run_all(st: Arc<AppState>) -> anyhow::Result<()> {
    for lb in &st.cfg.load_balancer {
        let ups = lb
            .upstreams
            .iter()
            .map(|u| UpstreamStatus {
                url: u.trim_end_matches('/').to_string(),
                healthy: true,
                fails: 0,
            })
            .collect();
        let rt = Arc::new(LbRuntime {
            name: lb.name.clone(),
            listen: lb.listen.clone(),
            health_path: lb.health_path.clone(),
            ups: RwLock::new(ups),
            rr: AtomicUsize::new(0),
            request_count: AtomicUsize::new(0),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(60))
                .pool_max_idle_per_host(64)
                .pool_idle_timeout(Duration::from_secs(90))
                .tcp_nodelay(true)
                .tcp_keepalive(Duration::from_secs(30))
                .build()?,
        });
        st.lb_runtimes.write().await.push(rt.clone());

        // 健康检查循环
        {
            let rt2 = rt.clone();
            let st2 = st.clone();
            let interval = lb.health_interval_secs.max(3);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(interval)).await;
                    health_check(&st2, &rt2).await;
                }
            });
        }

        // 代理服务
        let app = axum::Router::new()
            .fallback(proxy)
            .with_state(rt.clone());
        let listener = tokio::net::TcpListener::bind(&lb.listen).await?;
        tracing::info!("负载均衡 [{}] 监听 {} -> {} 个上游", lb.name, lb.listen, lb.upstreams.len());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("负载均衡服务退出: {e}");
            }
        });
    }
    Ok(())
}

async fn health_check(st: &Arc<AppState>, rt: &Arc<LbRuntime>) {
    let mut ups = rt.ups.write().await;
    for up in ups.iter_mut() {
        let url = format!("{}{}", up.url, rt.health_path);
        let ok = rt
            .client
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        let was = up.healthy;
        if ok {
            up.fails = 0;
            up.healthy = true;
            if !was {
                tracing::info!("LB [{}] 上游 {} 恢复健康", rt.name, up.url);
                crate::events::emit(st, "lb.up", &up.url, "info", &format!("负载均衡 [{}] 上游恢复", rt.name)).await;
            }
        } else {
            up.fails += 1;
            if up.fails >= 2 {
                up.healthy = false;
                if was {
                    tracing::warn!("LB [{}] 上游 {} 标记不健康", rt.name, up.url);
                    crate::events::emit(st, "lb.down", &up.url, "warn", &format!("负载均衡 [{}] 上游探活失败已摘除", rt.name)).await;
                }
            }
        }
    }
}

async fn proxy(State(rt): State<Arc<LbRuntime>>, req: Request) -> Response {
    rt.request_count.fetch_add(1, Ordering::SeqCst);
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap_or_default();

    let pq = parts
        .uri
        .path_and_query()
        .map(|x| x.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let n = rt.ups.read().await.len();
    if n == 0 {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "maylin: no upstream",
        )
            .into_response();
    }

    // 轮询选择健康上游；请求失败自动换下一个（覆盖健康检查间隔内的故障窗口）
    let mut last_err: Option<reqwest::Error> = None;
    let mut tried = 0;
    while tried < n {
        let ups = rt.ups.read().await;
        let idx = rt.rr.fetch_add(1, Ordering::SeqCst) % n;
        let upstream = ups[idx].url.clone();
        let healthy = ups[idx].healthy;
        drop(ups);
        if !healthy {
            tried += 1;
            continue;
        }
        tried += 1;
        let url = format!("{upstream}{pq}");

        let mut r = rt.client.request(parts.method.clone(), &url);
        for (k, v) in parts.headers.iter() {
            if !is_hop(k) {
                r = r.header(k.clone(), v.clone());
            }
        }
        r = r.header("x-forwarded-proto", "http");

        match r.body(bytes.to_vec()).send().await {
            Ok(resp) => {
                let status = resp.status();
                let mut builder = Response::builder().status(status.as_u16());
                for (k, v) in resp.headers().iter() {
                    if !is_hop(k) {
                        builder = builder.header(k.clone(), v.clone());
                    }
                }
                let body = resp.bytes().await.unwrap_or_default();
                return builder
                    .body(Body::from(body))
                    .unwrap_or_else(|_| axum::http::StatusCode::BAD_GATEWAY.into_response());
            }
            Err(e) => {
                tracing::warn!("LB [{}] 上游 {upstream} 请求失败: {e}，尝试下一个", rt.name);
                // 立即标记不健康，加速故障摘除
                let mut ups = rt.ups.write().await;
                if let Some(up) = ups.iter_mut().find(|u| u.url == upstream) {
                    up.fails += 1;
                    if up.fails >= 2 {
                        up.healthy = false;
                    }
                }
                drop(ups);
                last_err = Some(e);
            }
        }
    }

    (
        axum::http::StatusCode::BAD_GATEWAY,
        format!("maylin: all upstreams failed: {}", last_err.map(|e| e.to_string()).unwrap_or_default()),
    )
        .into_response()
}
