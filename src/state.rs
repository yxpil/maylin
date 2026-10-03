use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

use crate::balancer::LbRuntime;
use crate::config::{InstanceSpec, PluginSpec, RootConfig, ScheduleSpec};

/// API 角色（权限从高到低：Admin > Operator > Viewer）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Operator,
    Viewer,
}

impl Role {
    pub fn rank(&self) -> u8 {
        match self {
            Role::Admin => 3,
            Role::Operator => 2,
            Role::Viewer => 1,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "admin" => Some(Role::Admin),
            "operator" => Some(Role::Operator),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

/// 轻量运行指标
#[derive(Debug, Default)]
pub struct Metrics {
    pub api_requests: AtomicU64,
    pub lb_requests: AtomicU64,
    pub exec_runs: AtomicU64,
    pub auth_fails: AtomicU64,
}

/// 实例运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Stopped,
    Starting,
    Running,
    Exited,
    Failed,
    Unhealthy,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Stopped => "stopped",
            RunStatus::Starting => "starting",
            RunStatus::Running => "running",
            RunStatus::Exited => "exited",
            RunStatus::Failed => "failed",
            RunStatus::Unhealthy => "unhealthy",
        }
    }
    pub fn alive(&self) -> bool {
        matches!(
            self,
            RunStatus::Starting | RunStatus::Running | RunStatus::Unhealthy
        )
    }
}

/// 一行实例日志
#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub ts: String,
    /// out | err
    pub stream: String,
    pub text: String,
}

/// 发送给 supervisor 的控制指令
#[derive(Debug, Clone)]
pub enum Ctrl {
    Stop,
    /// 写入子进程 stdin 的一行（不含换行）
    Stdin(String),
}

pub struct Instance {
    pub spec: InstanceSpec,
    pub status: RwLock<RunStatus>,
    /// 是否期望处于运行中（stop 置 false，start 置 true），用于崩溃重启判断
    pub desired: AtomicBool,
    pub pid: std::sync::atomic::AtomicI32,
    pub restarts: AtomicU64,
    /// 防止并发 start 的锁
    pub start_lock: Mutex<()>,
    pub ctrl: RwLock<Option<mpsc::Sender<Ctrl>>>,
    pub output: broadcast::Sender<LogLine>,
    pub logs: RwLock<VecDeque<LogLine>>,
    pub started_at: RwLock<Option<DateTime<Local>>>,
    pub last_exit: RwLock<Option<String>>,
}

impl Instance {
    pub fn new(spec: InstanceSpec) -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        Arc::new(Self {
            spec,
            status: RwLock::new(RunStatus::Stopped),
            desired: AtomicBool::new(false),
            pid: std::sync::atomic::AtomicI32::new(-1),
            restarts: AtomicU64::new(0),
            start_lock: Mutex::new(()),
            ctrl: RwLock::new(None),
            output: tx,
            logs: RwLock::new(VecDeque::new()),
            started_at: RwLock::new(None),
            last_exit: RwLock::new(None),
        })
    }

    pub async fn status(&self) -> RunStatus {
        *self.status.read().await
    }

    pub async fn set_status(&self, s: RunStatus) {
        *self.status.write().await = s;
    }

    pub async fn set_last_exit(&self, s: String) {
        *self.last_exit.write().await = Some(s);
    }

    pub async fn push_log(
        &self,
        cap: usize,
        line: LogLine,
        wf: &Arc<Mutex<Option<tokio::fs::File>>>,
    ) {
        {
            let mut g = self.logs.write().await;
            g.push_back(line.clone());
            while g.len() > cap {
                g.pop_front();
            }
        }
        let _ = self.output.send(line.clone());
        let mut g = wf.lock().await;
        if let Some(f) = g.as_mut() {
            use tokio::io::AsyncWriteExt;
            let _ = f
                .write_all(format!("[{}] [{}] {}\n", line.ts, line.stream, line.text).as_bytes())
                .await;
        }
    }
}

pub struct AppState {
    pub cfg: RootConfig,
    /// 配置根目录（maylin.toml 所在目录的父目录）
    pub root_dir: PathBuf,
    pub data_dir: PathBuf,
    pub instances: RwLock<HashMap<String, Arc<Instance>>>,
    pub plugins: RwLock<HashMap<String, PluginSpec>>,
    pub schedules: RwLock<Vec<ScheduleSpec>>,
    pub lb_runtimes: RwLock<Vec<Arc<LbRuntime>>>,
    /// token -> 角色（auth.tokens = admin；auth.users 按配置）
    pub roles: HashMap<String, Role>,
    /// 事件环缓冲（详见 events.rs）
    pub events: RwLock<VecDeque<crate::events::Event>>,
    pub metrics: Metrics,
    /// 直连客户端（不走系统代理）：健康检查/集群内部通信等本机与内网流量
    pub local_client: reqwest::Client,
    pub client: reqwest::Client,
    pub started_at: DateTime<Utc>,
}

fn http_client(no_proxy: bool, timeout: u64) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .user_agent(concat!("maylin/", env!("CARGO_PKG_VERSION")));
    if no_proxy {
        b = b.no_proxy();
    }
    b.build().unwrap_or_default()
}

impl AppState {
    pub fn new(cfg: RootConfig, root_dir: PathBuf) -> anyhow::Result<Self> {
        let data_dir = root_dir.join(&cfg.node.data_dir);
        std::fs::create_dir_all(data_dir.join("logs"))?;

        // 构建 token -> 角色映射
        let mut roles = HashMap::new();
        for t in &cfg.auth.tokens {
            roles.insert(t.clone(), Role::Admin);
        }
        for u in &cfg.auth.users {
            if u.token.is_empty() {
                continue;
            }
            let role = Role::parse(&u.role).unwrap_or(Role::Viewer);
            roles.insert(u.token.clone(), role);
        }

        Ok(Self {
            cfg,
            root_dir,
            data_dir,
            instances: RwLock::new(HashMap::new()),
            plugins: RwLock::new(HashMap::new()),
            schedules: RwLock::new(Vec::new()),
            lb_runtimes: RwLock::new(Vec::new()),
            roles,
            events: RwLock::new(VecDeque::new()),
            metrics: Metrics::default(),
            local_client: http_client(true, 10),
            client: http_client(false, 10),
            started_at: Utc::now(),
        })
    }

    pub async fn get_instance(&self, name: &str) -> Option<Arc<Instance>> {
        self.instances.read().await.get(name).cloned()
    }

    pub async fn instance_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.instances.read().await.keys().cloned().collect();
        v.sort();
        v
    }

    /// 用于本节点 API 认证/集群互访的默认 token
    pub fn token(&self) -> &str {
        self.cfg
            .auth
            .tokens
            .first()
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    pub fn log_file_path(&self, name: &str) -> PathBuf {
        self.data_dir.join("logs").join(format!("{name}.log"))
    }
}
