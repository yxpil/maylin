use anyhow::{bail, Context, Result};
use clap::Subcommand;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;

#[derive(Subcommand, Debug)]
pub enum CtlCmd {
    /// 节点状态
    Status,
    /// 实例列表
    Instances,
    /// 启动实例
    Start { name: String },
    /// 停止实例
    Stop { name: String },
    /// 重启实例
    Restart { name: String },
    /// 查看日志
    Logs {
        name: String,
        #[arg(long, default_value_t = 100)]
        lines: usize,
        #[arg(long, help = "持续跟踪新日志")]
        follow: bool,
    },
    /// 交互式终端（接入实例 stdin/stdout，输入 /quit 退出）
    Terminal { name: String },
    /// 在节点上执行 shell 命令
    Exec {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// 集群聚合状态
    Cluster,
    /// 集群广播动作（start/stop/restart，target 可为实例名或 all）
    ClusterAction { action: String, target: String },
    /// 从本地 toml 文件创建实例
    Add { file: String },
    /// 重新扫描配置目录
    Reload,
    /// 负载均衡状态
    Lb,
    /// 最近事件（告警/审计日志）
    Events {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

pub async fn run(url: Option<String>, token: Option<String>, cmd: CtlCmd) -> Result<()> {
    let url = url
        .or_else(|| std::env::var("MAYLIN_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:7000".into());
    let token = token
        .or_else(|| std::env::var("MAYLIN_TOKEN").ok())
        .context("缺少访问令牌：使用 --token 或环境变量 MAYLIN_TOKEN")?;

    if let CtlCmd::Terminal { name } = cmd {
        return terminal(&url, &token, &name).await;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let base = url.trim_end_matches('/').to_string();

    match cmd {
        CtlCmd::Status => {
            let v = do_get(&client, &base, "/api/status", &token).await?;
            print_pretty(&v);
        }
        CtlCmd::Instances => {
            let v = do_get(&client, &base, "/api/instances", &token).await?;
            print_pretty(&v);
        }
        CtlCmd::Cluster => {
            let v = do_get(&client, &base, "/api/cluster/status", &token).await?;
            print_pretty(&v);
        }
        CtlCmd::Lb => {
            let v = do_get(&client, &base, "/api/lb/status", &token).await?;
            print_pretty(&v);
        }
        CtlCmd::Events { limit } => {
            let v = do_get(
                &client,
                &base,
                &format!("/api/events?limit={limit}"),
                &token,
            )
            .await?;
            if let Some(arr) = v["events"].as_array() {
                for e in arr {
                    let ts = e["ts"].as_str().unwrap_or("");
                    let kind = e["kind"].as_str().unwrap_or("");
                    let target = e["target"].as_str().unwrap_or("");
                    let level = e["level"].as_str().unwrap_or("info");
                    let msg = e["message"].as_str().unwrap_or("");
                    let (color, reset) = match level {
                        "error" => ("\x1b[31m", "\x1b[0m"),
                        "warn" => ("\x1b[33m", "\x1b[0m"),
                        _ => ("", ""),
                    };
                    println!("{color}[{ts}] [{kind}] {target}: {msg}{reset}");
                }
            }
        }
        CtlCmd::Start { name } => {
            let v = do_post(
                &client,
                &base,
                &format!("/api/instances/{name}/start"),
                &token,
            )
            .await?;
            print_pretty(&v);
        }
        CtlCmd::Stop { name } => {
            let v = do_post(
                &client,
                &base,
                &format!("/api/instances/{name}/stop"),
                &token,
            )
            .await?;
            print_pretty(&v);
        }
        CtlCmd::Restart { name } => {
            let v = do_post(
                &client,
                &base,
                &format!("/api/instances/{name}/restart"),
                &token,
            )
            .await?;
            print_pretty(&v);
        }
        CtlCmd::Logs {
            name,
            lines,
            follow,
        } => {
            let mut seen: usize = 0;
            let mut first = true;
            loop {
                let v = do_get(
                    &client,
                    &base,
                    &format!("/api/instances/{name}/logs?lines=100000"),
                    &token,
                )
                .await?;
                if let Some(arr) = v.as_array() {
                    let total = arr.len();
                    let start = if first {
                        first = false;
                        total.saturating_sub(lines)
                    } else {
                        seen.min(total)
                    };
                    for l in &arr[start..] {
                        let stream = l["stream"].as_str().unwrap_or("out");
                        let ts = l["ts"].as_str().unwrap_or("");
                        let text = l["text"].as_str().unwrap_or("");
                        if stream == "err" {
                            println!("\x1b[31m[{ts}] [err] {text}\x1b[0m");
                        } else {
                            println!("[{ts}] {text}");
                        }
                    }
                    seen = total;
                }
                if !follow {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        CtlCmd::Terminal { .. } => unreachable!(),
        CtlCmd::Exec { command } => {
            let v = client
                .post(format!("{base}/api/exec"))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "command": command.join(" ") }))
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            if let Some(s) = v["stdout"].as_str() {
                if !s.is_empty() {
                    print!("{s}");
                }
            }
            if let Some(s) = v["stderr"].as_str() {
                if !s.is_empty() {
                    eprint!("\x1b[31m{s}\x1b[0m");
                }
            }
            if let Some(code) = v["code"].as_i64() {
                std::process::exit(code as i32);
            }
        }
        CtlCmd::ClusterAction { action, target } => {
            let v = client
                .post(format!("{base}/api/cluster/action"))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "action": action, "target": target }))
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            print_pretty(&v);
        }
        CtlCmd::Add { file } => {
            let raw = std::fs::read_to_string(&file).context("读取实例配置失败")?;
            let f: toml::Value = toml::from_str(&raw)?;
            let spec = f.get("instance").context("文件缺少 [instance] 段")?;
            let v = client
                .post(format!("{base}/api/instances"))
                .bearer_auth(&token)
                .json(&spec)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            print_pretty(&v);
        }
        CtlCmd::Reload => {
            let v = do_post(&client, &base, "/api/reload", &token).await?;
            print_pretty(&v);
        }
    }
    Ok(())
}

async fn do_get(client: &reqwest::Client, base: &str, path: &str, token: &str) -> Result<Value> {
    let v = client
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    Ok(v)
}

async fn do_post(client: &reqwest::Client, base: &str, path: &str, token: &str) -> Result<Value> {
    let v = client
        .post(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    Ok(v)
}

fn print_pretty(v: &Value) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("输出失败: {e}"),
    }
}

async fn terminal(url: &str, token: &str, name: &str) -> Result<()> {
    let ws_url = format!(
        "{}/api/instances/{name}/terminal?token={token}",
        url.trim_end_matches('/').replacen("http", "ws", 1)
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(ws_url.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("连接终端失败: {e}"))?;
    println!("已接入实例 [{name}] 终端。输入将发送到该进程 stdin，/quit 退出。");
    println!("----------------------------------------------------------");

    let (mut write, mut read) = ws.split();

    // 读循环：打印实例输出
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = read.next().await {
            if msg.is_text() || msg.is_binary() {
                if let Ok(s) = msg.into_text() {
                    if let Ok(v) = serde_json::from_str::<Value>(s.as_str()) {
                        let stream = v["stream"].as_str().unwrap_or("out");
                        let text = v["text"].as_str().unwrap_or("");
                        if stream == "err" {
                            println!("\x1b[31m{text}\x1b[0m");
                        } else {
                            println!("{text}");
                        }
                    } else {
                        println!("{}", s.as_str());
                    }
                }
            }
        }
    });

    // 写循环：本地 stdin → 实例 stdin
    use tokio::io::AsyncBufReadExt;
    let stdin = tokio::io::stdin();
    let mut lines = tokio::io::BufReader::new(stdin).lines();
    let mut quit = false;
    loop {
        tokio::select! {
            l = lines.next_line() => {
                match l {
                    Ok(Some(line)) => {
                        if line.trim() == "/quit" {
                            quit = true;
                            break;
                        }
                        if write.send(tokio_tungstenite::tungstenite::Message::text(line)).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let _ = write.close().await;
    reader.abort();
    if quit {
        println!("\n终端已退出。");
        Ok(())
    } else {
        println!("\n终端连接已断开。");
        bail!("终端连接已断开")
    }
}
