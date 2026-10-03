use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::balancer::LbSnapshot;
use crate::config::InstanceSpec;
use crate::process;
use crate::state::{AppState, Instance, LogLine};

// ---------------------------------------------------------------------------
// 错误类型
// ---------------------------------------------------------------------------

pub struct ApiError {
    status: StatusCode,
    msg: String,
}

impl ApiError {
    #[allow(dead_code)]
    pub fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            msg: msg.into(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            msg: msg.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: format!("{e:#}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.msg }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Token 认证 + RBAC 授权中间件
// ---------------------------------------------------------------------------

/// 路径 → 所需最低角色；返回 false 表示该角色不可访问
fn authorized(role: crate::state::Role, method: &str, path: &str) -> bool {
    use crate::state::Role;
    match method {
        "GET" | "HEAD" => {
            // 终端可写 stdin，属 operator 权限
            if path.ends_with("/terminal") {
                role.rank() >= Role::Operator.rank()
            } else {
                true
            }
        }
        "POST" => {
            if path.ends_with("/start")
                || path.ends_with("/stop")
                || path.ends_with("/restart")
                || path == "/api/reload"
            {
                role.rank() >= Role::Operator.rank()
            } else {
                // 创建/删除实例、exec、cluster/action 等敏感操作
                role.rank() >= Role::Admin.rank()
            }
        }
        // DELETE 等
        _ => role.rank() >= Role::Admin.rank(),
    }
}

async fn auth(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        // WebSocket / 简单客户端可使用 ?token= 查询参数
        .or_else(|| {
            req.uri().query().and_then(|q| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("token=").map(|s| s.to_string()))
            })
        });

    let role = token.as_ref().and_then(|t| st.roles.get(t)).copied();
    let Some(role) = role else {
        st.metrics
            .auth_fails
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::events::emit_auth_fail(
            &st,
            format!(
                "认证失败: {} {} (remote token 有效性校验未通过)",
                req.method(),
                req.uri().path()
            ),
        )
        .await;
        return ApiError {
            status: StatusCode::UNAUTHORIZED,
            msg: "无效或缺失的 Token（Authorization: Bearer <token> 或 ?token=）".into(),
        }
        .into_response();
    };

    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    if !authorized(role, &method, &path) {
        return ApiError {
            status: StatusCode::FORBIDDEN,
            msg: format!(
                "权限不足：当前角色 {} 无权执行 {} {}",
                role.as_str(),
                method,
                path
            ),
        }
        .into_response();
    }

    st.metrics
        .api_requests
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    next.run(req).await
}

// ---------------------------------------------------------------------------
// 视图
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct InstanceView {
    name: String,
    plugin: String,
    status: String,
    pid: Option<i32>,
    port: Option<u16>,
    script: Option<String>,
    autostart: bool,
    restart_policy: String,
    restarts: u64,
    started_at: Option<String>,
    last_exit: Option<String>,
    log_file: String,
}

async fn view(st: &Arc<AppState>, inst: &Arc<Instance>) -> InstanceView {
    let status = inst.status().await;
    let pid = inst.pid.load(std::sync::atomic::Ordering::SeqCst);
    InstanceView {
        name: inst.spec.name.clone(),
        plugin: inst.spec.plugin.clone(),
        status: status.as_str().to_string(),
        pid: if pid > 0 { Some(pid) } else { None },
        port: inst.spec.port,
        script: inst.spec.script.clone(),
        autostart: inst.spec.autostart,
        restart_policy: inst.spec.restart_policy.as_str().to_string(),
        restarts: inst.restarts.load(std::sync::atomic::Ordering::SeqCst),
        started_at: inst
            .started_at
            .read()
            .await
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
        last_exit: inst.last_exit.read().await.clone(),
        log_file: st.log_file_path(&inst.spec.name).display().to_string(),
    }
}

async fn get_inst(st: &Arc<AppState>, name: &str) -> ApiResult<Arc<Instance>> {
    st.get_instance(name)
        .await
        .ok_or_else(|| ApiError::not_found(format!("实例 {name} 不存在")))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn status(State(st): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let mut instances = Vec::new();
    for name in st.instance_names().await {
        if let Some(i) = st.get_instance(&name).await {
            instances.push(json!(view(&st, &i).await));
        }
    }
    let mut lbs = Vec::new();
    for rt in st.lb_runtimes.read().await.iter() {
        lbs.push(json!(rt.snapshot().await));
    }
    Ok(Json(json!({
        "node": st.cfg.node.name,
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": st.started_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        "uptime_secs": (chrono::Utc::now() - st.started_at).num_seconds(),
        "listen": st.cfg.node.listen,
        "cluster_peers": st.cfg.cluster.peers,
        "schedules": st.schedules.read().await.len(),
        "load_balancers": lbs,
        "metrics": {
            "api_requests": st.metrics.api_requests.load(std::sync::atomic::Ordering::SeqCst),
            "lb_requests": st.metrics.lb_requests.load(std::sync::atomic::Ordering::SeqCst),
            "exec_runs": st.metrics.exec_runs.load(std::sync::atomic::Ordering::SeqCst),
            "auth_fails": st.metrics.auth_fails.load(std::sync::atomic::Ordering::SeqCst),
        },
        "alert": {
            "enabled": st.cfg.alert.enabled,
            "webhook": !st.cfg.alert.webhook_url.is_empty(),
        },
        "instances": instances,
    })))
}

async fn list_instances(State(st): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let mut out = Vec::new();
    for name in st.instance_names().await {
        if let Some(i) = st.get_instance(&name).await {
            out.push(json!(view(&st, &i).await));
        }
    }
    Ok(Json(json!({ "instances": out })))
}

async fn get_instance(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let inst = get_inst(&st, &name).await?;
    Ok(Json(json!(view(&st, &inst).await)))
}

async fn create_instance(
    State(st): State<Arc<AppState>>,
    Json(spec): Json<InstanceSpec>,
) -> ApiResult<Json<Value>> {
    let msg = process::create_instance(&st, spec)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}

async fn remove_instance(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let msg = process::remove_instance(&st, &name)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}

async fn start_instance(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let msg = process::start_instance(&st, &name)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}

async fn stop_instance(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let msg = process::stop_instance(&st, &name)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}

async fn restart_instance(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let msg = process::restart_instance(&st, &name)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}

async fn logs(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<LogQuery>,
) -> ApiResult<Json<Vec<LogLine>>> {
    let inst = get_inst(&st, &name).await?;
    let g = inst.logs.read().await;
    let n = q.lines.unwrap_or(100).min(g.len().max(1));
    let start = g.len().saturating_sub(n);
    Ok(Json(g.iter().skip(start).cloned().collect()))
}

#[derive(Deserialize)]
struct LogQuery {
    lines: Option<usize>,
}

/// WebSocket 终端：接入实例 stdin/stdout
async fn terminal(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let inst = get_inst(&st, &name).await?;
    Ok(ws.on_upgrade(move |sock| terminal_sock(sock, inst)))
}

async fn terminal_sock(mut sock: WebSocket, inst: Arc<Instance>) {
    // 回放最近 50 行
    {
        let g = inst.logs.read().await;
        let start = g.len().saturating_sub(50);
        for line in g.iter().skip(start) {
            if send_log(&mut sock, line).await.is_err() {
                return;
            }
        }
    }
    let mut rx = inst.output.subscribe();
    loop {
        tokio::select! {
            l = rx.recv() => {
                match l {
                    Ok(line) => { if send_log(&mut sock, &line).await.is_err() { break; } }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            m = sock.recv() => {
                match m {
                    Some(Ok(Message::Text(t))) => {
                        let s = t.as_str().trim().to_string();
                        if s == "/quit" { break; }
                        if let Some(tx) = inst.ctrl.read().await.clone() {
                            let _ = tx.send(crate::state::Ctrl::Stdin(s)).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

async fn send_log(sock: &mut WebSocket, line: &LogLine) -> Result<(), axum::Error> {
    let text = serde_json::to_string(line).unwrap_or_default();
    sock.send(Message::text(text)).await
}

#[derive(Deserialize)]
struct ExecReq {
    command: String,
    #[serde(default = "default_exec_timeout")]
    timeout_secs: u64,
}

fn default_exec_timeout() -> u64 {
    30
}

/// 在节点上执行 shell 命令（admin only；并发上限 4 防止 fork 风暴）
async fn exec(State(st): State<Arc<AppState>>, Json(req): Json<ExecReq>) -> ApiResult<Json<Value>> {
    static EXEC_SEM: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let _permit = EXEC_SEM
        .acquire()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("exec 信号量不可用: {e}")))?;
    st.metrics
        .exec_runs
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    crate::events::emit(
        &st,
        "exec.run",
        "-",
        "info",
        &format!(
            "执行命令: {}",
            req.command.chars().take(120).collect::<String>()
        ),
    )
    .await;

    let command = req.command;
    let secs = req.timeout_secs.min(120);
    let out = tokio::task::spawn_blocking(move || {
        process::exec_command_blocking(&command, Duration::from_secs(secs))
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!("执行任务失败: {e}")))?;

    if out.timed_out {
        return Ok(Json(
            json!({ "timeout": true, "message": "命令执行超时已被终止" }),
        ));
    }
    Ok(Json(json!({
        "code": out.code,
        "stdout": out.stdout,
        "stderr": out.stderr,
    })))
}

async fn plugins(State(st): State<Arc<AppState>>) -> Json<Value> {
    let g = st.plugins.read().await;
    Json(json!({ "plugins": g.values().collect::<Vec<_>>() }))
}

async fn schedules(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "schedules": st.schedules.read().await.clone() }))
}

async fn reload(State(st): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let msg = process::load_configs(&st, true)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}

async fn lb_status(State(st): State<Arc<AppState>>) -> Json<Vec<LbSnapshot>> {
    let mut out = Vec::new();
    for rt in st.lb_runtimes.read().await.iter() {
        out.push(rt.snapshot().await);
    }
    Json(out)
}

async fn cluster_status_h(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(crate::cluster::cluster_status(&st).await)
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<usize>,
}

async fn events(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> Json<Value> {
    let g = st.events.read().await;
    let n = q.limit.unwrap_or(100).min(g.len().max(1));
    let start = g.len().saturating_sub(n);
    let items: Vec<crate::events::Event> = g.iter().skip(start).cloned().collect();
    Json(json!({ "events": items, "total": g.len() }))
}

#[derive(Deserialize)]
struct ClusterAction {
    action: String,
    target: String,
}

async fn cluster_action_h(
    State(st): State<Arc<AppState>>,
    Json(req): Json<ClusterAction>,
) -> Json<Value> {
    Json(crate::cluster::fanout_action(&st, &req.action, &req.target).await)
}

// ---------------------------------------------------------------------------
// 路由
// ---------------------------------------------------------------------------

pub fn build_router(st: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/api/status", get(status))
        .route("/api/instances", get(list_instances).post(create_instance))
        .route(
            "/api/instances/{name}",
            get(get_instance).delete(remove_instance),
        )
        .route("/api/instances/{name}/start", post(start_instance))
        .route("/api/instances/{name}/stop", post(stop_instance))
        .route("/api/instances/{name}/restart", post(restart_instance))
        .route("/api/instances/{name}/logs", get(logs))
        .route("/api/instances/{name}/terminal", get(terminal))
        .route("/api/exec", post(exec))
        .route("/api/plugins", get(plugins))
        .route("/api/schedules", get(schedules))
        .route("/api/reload", post(reload))
        .route("/api/lb/status", get(lb_status))
        .route("/api/cluster/status", get(cluster_status_h))
        .route("/api/cluster/action", post(cluster_action_h))
        .route("/api/events", get(events))
        .layer(middleware::from_fn_with_state(st.clone(), auth));

    Router::new().merge(protected).with_state(st)
}
