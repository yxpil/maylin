use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 根配置 maylin.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RootConfig {
    pub node: NodeCfg,
    #[serde(default)]
    pub auth: AuthCfg,
    #[serde(default)]
    pub alert: AlertCfg,
    #[serde(default)]
    pub cluster: ClusterCfg,
    #[serde(default, rename = "load_balancer")]
    pub load_balancer: Vec<LbCfg>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeCfg {
    pub name: String,
    /// API 监听地址，如 0.0.0.0:7000
    pub listen: String,
    /// 数据目录（相对 root 目录），存放运行日志等
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// 内存中每实例保留的日志行数
    #[serde(default = "default_log_lines")]
    pub log_buffer_lines: usize,
}

fn default_data_dir() -> String {
    "data".into()
}
fn default_log_lines() -> usize {
    2000
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AuthCfg {
    /// 访问 API 所需的 Bearer Token（= admin 全权限）；为空时首次启动自动生成
    #[serde(default)]
    pub tokens: Vec<String>,
    /// 细粒度角色用户（可选）：admin / operator / viewer
    #[serde(default)]
    pub users: Vec<UserEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserEntry {
    #[serde(default)]
    pub name: String,
    pub token: String,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "viewer".into()
}

/// Webhook 告警配置
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AlertCfg {
    pub enabled: bool,
    /// 事件触发时 POST JSON 到该地址
    pub webhook_url: String,
    /// 只推送这些事件类型；留空 = 全部。如 instance.crash / lb.down / auth.fail
    pub events: Vec<String>,
    pub timeout_secs: u64,
}

impl Default for AlertCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            webhook_url: String::new(),
            events: Vec::new(),
            timeout_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClusterCfg {
    #[serde(default)]
    pub enabled: bool,
    /// 集群中其他节点的 API 地址，如 http://192.168.1.10:7000
    #[serde(default)]
    pub peers: Vec<String>,
    /// 节点间互访使用的 Token（必须同时存在于各节点 auth.tokens 中）
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LbCfg {
    pub name: String,
    /// 负载均衡监听地址，如 0.0.0.0:8000
    pub listen: String,
    /// 上游地址，可混合本机端口与远端节点端口
    pub upstreams: Vec<String>,
    #[serde(default = "default_health_path")]
    pub health_path: String,
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
}

fn default_health_path() -> String {
    "/health".into()
}
fn default_health_interval() -> u64 {
    10
}

// ---------------------------------------------------------------------------
// 插件（运行时适配器）plugins/*.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginFile {
    pub plugin: PluginSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginSpec {
    pub name: String,
    /// 可执行程序，支持 {script} {port} {name} 占位符
    pub command: String,
    /// 参数模板，支持占位符；实例自身的 args 会追加在后面
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 健康检查 URL 模板，如 http://127.0.0.1:{port}/health
    #[serde(default)]
    pub health_url: Option<String>,
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    /// 停止时的宽限秒数，超时后强杀
    #[serde(default = "default_grace")]
    pub grace_secs: u64,
    /// 优雅停止时向子进程 stdin 写入的行（可选）
    #[serde(default)]
    pub shutdown_line: Option<String>,
}

fn default_grace() -> u64 {
    5
}

// ---------------------------------------------------------------------------
// 实例 instances/*.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstanceFile {
    pub instance: InstanceSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstanceSpec {
    pub name: String,
    /// 引用插件名（node / python / binary / 自定义）
    pub plugin: String,
    /// 工作目录（相对 root，可绝对）
    #[serde(default)]
    pub workdir: Option<String>,
    /// 脚本路径（相对 workdir），binary 类插件可直接放可执行文件路径
    #[serde(default)]
    pub script: Option<String>,
    /// 追加参数，支持 {port} {script} {name} 占位符
    #[serde(default)]
    pub args: Vec<String>,
    /// 本实例独占端口（会注入 PORT 环境变量并参与占位符替换）
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// 覆盖插件的健康检查 URL 模板
    #[serde(default)]
    pub health_url: Option<String>,
    /// 覆盖插件的优雅停止行
    #[serde(default)]
    pub shutdown_line: Option<String>,
}

fn default_retries() -> u32 {
    5
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RestartPolicy {
    /// 不自动重启
    #[default]
    No,
    /// 非零退出码时重启
    #[serde(rename = "on-failure")]
    OnFailure,
    /// 总是重启（除非主动 stop）
    Always,
}

impl RestartPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            RestartPolicy::No => "no",
            RestartPolicy::OnFailure => "on-failure",
            RestartPolicy::Always => "always",
        }
    }
}

// ---------------------------------------------------------------------------
// 定时任务 schedules/*.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduleFile {
    pub schedule: ScheduleSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduleSpec {
    pub name: String,
    /// 标准 5 段 cron：分 时 日 月 周，如 "0 3 * * *"（每天 03:00）
    pub cron: String,
    /// start | stop | restart
    pub action: String,
    /// 实例名或 "all"
    pub target: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// 加载器
// ---------------------------------------------------------------------------

pub fn load_root(path: &Path) -> Result<RootConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置失败: {}", path.display()))?;
    let cfg: RootConfig =
        toml::from_str(&raw).with_context(|| format!("解析配置失败: {}", path.display()))?;
    Ok(cfg)
}

fn read_tomls(dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
        .collect();
    entries.sort();
    for p in entries {
        let raw = std::fs::read_to_string(&p)
            .with_context(|| format!("读取 {} 失败", p.display()))?;
        out.push((p, raw));
    }
    Ok(out)
}

pub fn load_plugins(dir: &Path) -> Result<Vec<PluginSpec>> {
    let mut out = Vec::new();
    for (p, raw) in read_tomls(dir)? {
        let f: PluginFile = toml::from_str(&raw).with_context(|| format!("解析 {} 失败", p.display()))?;
        out.push(f.plugin);
    }
    Ok(out)
}

pub fn load_instances(dir: &Path) -> Result<Vec<InstanceSpec>> {
    let mut out = Vec::new();
    for (p, raw) in read_tomls(dir)? {
        let f: InstanceFile =
            toml::from_str(&raw).with_context(|| format!("解析 {} 失败", p.display()))?;
        out.push(f.instance);
    }
    Ok(out)
}

pub fn load_schedules(dir: &Path) -> Result<Vec<ScheduleSpec>> {
    let mut out = Vec::new();
    for (p, raw) in read_tomls(dir)? {
        let f: ScheduleFile =
            toml::from_str(&raw).with_context(|| format!("解析 {} 失败", p.display()))?;
        out.push(f.schedule);
    }
    Ok(out)
}

/// 将相对路径映射到 root 目录下
pub fn resolve_path(root: &Path, p: &Option<String>) -> Option<PathBuf> {
    p.as_ref().map(|s| {
        let pb = PathBuf::from(s);
        if pb.is_absolute() {
            pb
        } else {
            root.join(pb)
        }
    })
}

// ---------------------------------------------------------------------------
// 首次运行自动生成默认配置
// ---------------------------------------------------------------------------

pub fn gen_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..40).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect()
}

pub fn bootstrap_if_missing(config_path: &Path) -> Result<bool> {
    if config_path.exists() {
        return Ok(false);
    }
    let root = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    std::fs::create_dir_all(root.join("instances"))?;
    std::fs::create_dir_all(root.join("plugins"))?;
    std::fs::create_dir_all(root.join("schedules"))?;
    std::fs::create_dir_all(root.join("data/logs"))?;

    let token = gen_token();

    let maylin_toml = format!(
        r#"# Maylin 节点主配置
[node]
name = "node-1"
listen = "0.0.0.0:7000"      # API / WebSocket 终端监听地址
data_dir = "data"            # 运行日志目录
log_buffer_lines = 2000      # 内存中保留的每实例日志行数

[auth]
# 访问 API / ctl 的 Bearer Token（= admin 全权限，可配置多个）
tokens = ["{token}"]

# 细粒度权限（可选）：admin=全部 / operator=实例操作+终端+reload / viewer=只读
# [[auth.users]]
# name = "ops"
# token = "另一个随机串"
# role = "operator"

[alert]
# 事件告警：事件写入 data/events.jsonl，并可 POST JSON 到 webhook
enabled = false
# webhook_url = "https://example.com/hook"
# events = ["instance.crash", "instance.fail", "lb.down", "auth.fail"]  # 留空=全部
timeout_secs = 5

[cluster]
enabled = false
# peers = ["http://192.168.1.10:7000", "http://192.168.1.11:7000"]
# token = "集群互访令牌，需同时存在于各节点 auth.tokens"

# 内置负载均衡（反向代理），上游可混合本机实例与远端节点
[[load_balancer]]
name = "web"
listen = "0.0.0.0:8000"
upstreams = [
    "http://127.0.0.1:3001",
    "http://127.0.0.1:3002",
]
health_path = "/health"
health_interval_secs = 10
"#,
        token = token
    );
    std::fs::write(config_path, maylin_toml)?;

    let plugins: &[(&str, String)] = &[
        (
            "node.toml",
            r#"[plugin]
name = "node"
command = "node"             # 可执行程序，支持 {script} {port} {name} 占位符
args = ["{script}"]
health_interval_secs = 10
grace_secs = 5
# shutdown_line = "exit"     # 优雅停止时写入 stdin 的行
"#
            .into(),
        ),
        (
            "python.toml",
            r#"[plugin]
name = "python"
command = "python"
args = ["{script}"]
health_interval_secs = 10
grace_secs = 5
"#
            .into(),
        ),
        (
            "binary.toml",
            r#"[plugin]
name = "binary"
command = "{script}"         # 直接运行可执行文件
args = []
health_interval_secs = 10
grace_secs = 5
"#
            .into(),
        ),
    ];
    for (name, body) in plugins {
        std::fs::write(root.join("plugins").join(name), body)?;
    }

    std::fs::write(
        root.join("instances").join("demo-node.toml"),
        r#"[instance]
name = "demo-node"
plugin = "node"
workdir = "../demo"
script = "server.js"
args = ["--port", "{port}"]
port = 3001                  # 本实例独占端口
autostart = false
restart_policy = "on-failure"
max_retries = 5
health_url = "http://127.0.0.1:{port}/health"
"#,
    )?;

    std::fs::write(
        root.join("instances").join("demo-python.toml"),
        r#"[instance]
name = "demo-python"
plugin = "python"
workdir = "../demo"
script = "app.py"
args = ["--port", "{port}"]
port = 3002
autostart = false
restart_policy = "on-failure"
max_retries = 5
health_url = "http://127.0.0.1:{port}/health"
"#,
    )?;

    std::fs::write(
        root.join("schedules").join("example-nightly-restart.toml"),
        r#"[schedule]
name = "nightly-restart"
cron = "0 3 * * *"           # 分 时 日 月 周：每天 03:00
action = "restart"           # start | stop | restart
target = "demo-node"         # 实例名或 "all"
enabled = false
"#,
    )?;

    Ok(true)
}
