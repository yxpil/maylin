mod api;
mod balancer;
mod cluster;
mod config;
mod ctl;
mod events;
mod persist;
mod plugin;
mod process;
mod scheduler;
mod state;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "maylin",
    version,
    about = "Maylin - 无 Docker 的 Rust 服务集群管理器"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动节点守护进程（进程管理 + API + 终端 + 定时任务 + 负载均衡）
    Node {
        #[arg(long, default_value = "config/maylin.toml", help = "主配置文件路径")]
        config: String,
    },
    /// 管理客户端
    Ctl {
        #[arg(long, help = "节点 API 地址，默认 http://127.0.0.1:7000")]
        url: Option<String>,
        #[arg(long, help = "访问令牌（也可用环境变量 MAYLIN_TOKEN）")]
        token: Option<String>,
        #[command(subcommand)]
        cmd: ctl::CtlCmd,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "maylin=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let r = match cli.cmd {
        Commands::Node { config } => run_node(config).await,
        Commands::Ctl { url, token, cmd } => ctl::run(url, token, cmd).await,
    };
    if let Err(e) = r {
        eprintln!("\x1b[31m错误: {e:#}\x1b[0m");
        std::process::exit(1);
    }
}

async fn run_node(config_path: String) -> Result<()> {
    let cfg_path = PathBuf::from(&config_path);
    let fresh = config::bootstrap_if_missing(&cfg_path)?;
    if fresh {
        eprintln!(
            "首次运行：已在 {} 生成默认配置（含随机访问令牌，见 tokens 字段）",
            cfg_path.display()
        );
    }
    let mut cfg = config::load_root(&cfg_path)?;
    if cfg.auth.tokens.is_empty() {
        let t = config::gen_token();
        eprintln!("警告: auth.tokens 为空，本次会话使用临时令牌（请写入配置文件）: {t}");
        cfg.auth.tokens.push(t);
    }
    let root = cfg_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let listen = cfg.node.listen.clone();
    let st = Arc::new(state::AppState::new(cfg, root)?);

    // 装载插件 / 实例 / 定时任务
    process::load_configs(&st, false)
        .await
        .context("装载配置目录失败")?;

    // 定时任务与负载均衡
    scheduler::spawn_all(st.clone()).await;
    balancer::run_all(st.clone()).await?;

    // 运行状态持久化（每 10s 快照 data/state.json）
    persist::spawn_save_loop(st.clone()).await;

    // 优雅退出：Ctrl-C（全平台）+ SIGTERM（unix）
    spawn_shutdown(st.clone());
    #[cfg(unix)]
    {
        let st2 = st.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut s) = signal(SignalKind::terminate()) {
                s.recv().await;
                eprintln!("\n收到 SIGTERM，正在停止全部实例...");
                process::stop_all(&st2).await;
                events::emit(&st2, "node.stop", "-", "info", "收到 SIGTERM，节点退出").await;
                persist::save(&st2).await;
                std::process::exit(0);
            }
        });
    }

    let app = api::build_router(st.clone());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("监听 {listen} 失败"))?;
    eprintln!(
        "Maylin 节点 [{}] v{} 已启动: API={} (实例 {} 个, 集群 peers {} 个)",
        st.cfg.node.name,
        env!("CARGO_PKG_VERSION"),
        listen,
        st.instance_names().await.len(),
        st.cfg.cluster.peers.len()
    );
    events::emit(
        &st,
        "node.start",
        "-",
        "info",
        &format!("节点启动 v{} listen={listen}", env!("CARGO_PKG_VERSION")),
    )
    .await;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Ctrl-C 优雅退出：停全部实例 → 落盘快照 → 退出
fn spawn_shutdown(st: Arc<state::AppState>) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n收到 Ctrl-C，正在停止全部实例...");
            process::stop_all(&st).await;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while std::time::Instant::now() < deadline {
                let mut alive = 0;
                for n in st.instance_names().await {
                    if let Some(i) = st.get_instance(&n).await {
                        if i.status().await.alive() {
                            alive += 1;
                        }
                    }
                }
                if alive == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            events::emit(&st, "node.stop", "-", "info", "收到 Ctrl-C，节点退出").await;
            persist::save(&st).await;
            std::process::exit(0);
        }
    });
}
