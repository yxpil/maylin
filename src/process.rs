use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use chrono::Local;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use crate::config::{InstanceFile, InstanceSpec};
use crate::plugin::ResolvedLaunch;
use crate::state::{AppState, Ctrl, Instance, LogLine, RunStatus};

// ---------------------------------------------------------------------------
// 跨平台匿名管道
// Windows 使用经典 CreatePipe（本机环境中 Rust 命名管道实现不可用），
// 且读取端不依赖 EOF（安全层可能额外持有句柄），改用 PeekNamedPipe 轮询 + 退出标志。
// ---------------------------------------------------------------------------

pub mod pipes {
    use std::io::{self, Read, Write};
    use std::process::Stdio;

    /// 管道读取端（Windows 下支持 available() 探测可读字节数）
    pub struct PipeReader {
        f: std::fs::File,
    }

    impl Read for PipeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.f.read(buf)
        }
    }

    impl PipeReader {
        #[cfg(windows)]
        pub fn available(&self) -> io::Result<u32> {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::Pipes::PeekNamedPipe;
            let mut avail: u32 = 0;
            let h = self.f.as_raw_handle() as *mut std::ffi::c_void;
            if unsafe { PeekNamedPipe(h, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(avail)
        }
        #[cfg(not(windows))]
        pub fn available(&self) -> io::Result<u32> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "not supported"))
        }
    }

    /// 管道写入端
    pub struct PipeWriter {
        f: std::fs::File,
    }

    impl Write for PipeWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.f.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.f.flush()
        }
    }

    /// (父进程读端, 子进程 stdout/stderr 应使用的 Stdio)
    pub fn output_pipe() -> io::Result<(PipeReader, Stdio)> {
        #[cfg(windows)]
        {
            use std::os::windows::io::{FromRawHandle, RawHandle};
            let (r, w) = create_pipe(false, true)?;
            Ok((
                PipeReader { f: unsafe { std::fs::File::from_raw_handle(r as RawHandle) } },
                unsafe { Stdio::from_raw_handle(w as RawHandle) },
            ))
        }
        #[cfg(not(windows))]
        {
            let (r, w) = std::io::pipe()?;
            let f = unsafe { std::fs::File::from_raw_fd(r.into_raw_fd()) };
            Ok((PipeReader { f }, Stdio::from(w)))
        }
    }

    /// (父进程写端, 子进程 stdin 应使用的 Stdio)
    pub fn input_pipe() -> io::Result<(PipeWriter, Stdio)> {
        #[cfg(windows)]
        {
            use std::os::windows::io::{FromRawHandle, RawHandle};
            let (r, w) = create_pipe(true, false)?;
            Ok((
                PipeWriter { f: unsafe { std::fs::File::from_raw_handle(w as RawHandle) } },
                unsafe { Stdio::from_raw_handle(r as RawHandle) },
            ))
        }
        #[cfg(not(windows))]
        {
            let (r, w) = std::io::pipe()?;
            let f = unsafe { std::fs::File::from_raw_fd(w.into_raw_fd()) };
            Ok((PipeWriter { f }, Stdio::from(r)))
        }
    }

    #[cfg(windows)]
    fn create_pipe(
        inherit_read: bool,
        inherit_write: bool,
    ) -> io::Result<(*mut std::ffi::c_void, *mut std::ffi::c_void)> {
        use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Pipes::CreatePipe;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let mut r: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut w: *mut std::ffi::c_void = std::ptr::null_mut();
        if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if !inherit_read {
            unsafe { set_handle_info(r, HANDLE_FLAG_INHERIT, 0) };
        }
        if !inherit_write {
            unsafe { set_handle_info(w, HANDLE_FLAG_INHERIT, 0) };
        }
        Ok((r, w))
    }

    #[cfg(windows)]
    unsafe fn set_handle_info(h: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32 {
        use windows_sys::Win32::Foundation::SetHandleInformation;
        SetHandleInformation(h, mask, flags)
    }

    #[cfg(unix)]
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
}

type SharedChild = Arc<Mutex<Option<std::process::Child>>>;
type SharedStdin = Arc<Mutex<Option<pipes::PipeWriter>>>;

// ---------------------------------------------------------------------------
// 生命周期管理 API
// ---------------------------------------------------------------------------

/// 启动实例（已运行则报错）
pub async fn start_instance(st: &Arc<AppState>, name: &str) -> Result<String> {
    let inst = st
        .get_instance(name)
        .await
        .ok_or_else(|| anyhow!("实例 {name} 不存在"))?;
    let _guard = inst.start_lock.lock().await;
    let cur = inst.status().await;
    if cur.alive() {
        bail!("实例 {name} 已在运行（{}）", cur.as_str());
    }
    let launch = {
        let plugins = st.plugins.read().await;
        crate::plugin::resolve(&st.root_dir, &plugins, &inst.spec)?
    };
    inst.set_status(RunStatus::Starting).await;
    inst.desired.store(true, Ordering::SeqCst);
    inst.restarts.store(0, Ordering::SeqCst);
    tokio::spawn(run_lifecycle(st.clone(), inst.clone(), launch));
    Ok(format!("{name} 启动中 (port={:?})", inst.spec.port))
}

/// 停止实例（优雅 → 超时强杀）
pub async fn stop_instance(st: &Arc<AppState>, name: &str) -> Result<String> {
    let inst = st
        .get_instance(name)
        .await
        .ok_or_else(|| anyhow!("实例 {name} 不存在"))?;
    let cur = inst.status().await;
    if !cur.alive() {
        bail!("实例 {name} 未在运行（{}）", cur.as_str());
    }
    inst.desired.store(false, Ordering::SeqCst);
    if let Some(tx) = inst.ctrl.read().await.clone() {
        let _ = tx.send(Ctrl::Stop).await;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if !inst.status().await.alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(format!("{name} 已停止"))
}

pub async fn restart_instance(st: &Arc<AppState>, name: &str) -> Result<String> {
    let inst = st
        .get_instance(name)
        .await
        .ok_or_else(|| anyhow!("实例 {name} 不存在"))?;
    if inst.status().await.alive() {
        stop_instance(st, name).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    start_instance(st, name).await
}

/// 停止全部实例（Ctrl-C 时调用）
pub async fn stop_all(st: &Arc<AppState>) {
    let names = st.instance_names().await;
    for n in names {
        if let Some(inst) = st.get_instance(&n).await {
            if inst.status().await.alive() {
                inst.desired.store(false, Ordering::SeqCst);
                if let Some(tx) = inst.ctrl.read().await.clone() {
                    let _ = tx.send(Ctrl::Stop).await;
                }
            }
        }
    }
}

pub async fn remove_instance(st: &Arc<AppState>, name: &str) -> Result<String> {
    let inst = st
        .get_instance(name)
        .await
        .ok_or_else(|| anyhow!("实例 {name} 不存在"))?;
    if inst.status().await.alive() {
        stop_instance(st, name).await?;
    }
    st.instances.write().await.remove(name);
    let f = st.root_dir.join("instances").join(format!("{name}.toml"));
    if f.exists() {
        let _ = std::fs::remove_file(f);
    }
    Ok(format!("{name} 已删除"))
}

pub async fn create_instance(st: &Arc<AppState>, spec: InstanceSpec) -> Result<String> {
    if spec.name.is_empty()
        || !spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("实例名只能包含字母/数字/-/_");
    }
    if st.get_instance(&spec.name).await.is_some() {
        bail!("实例 {} 已存在", spec.name);
    }
    {
        let plugins = st.plugins.read().await;
        crate::plugin::resolve(&st.root_dir, &plugins, &spec)?;
    }
    let path = st
        .root_dir
        .join("instances")
        .join(format!("{}.toml", spec.name));
    let body = toml::to_string_pretty(&InstanceFile {
        instance: spec.clone(),
    })?;
    std::fs::write(&path, body)?;
    let autostart = spec.autostart;
    let name = spec.name.clone();
    st.instances
        .write()
        .await
        .insert(name.clone(), Instance::new(spec));
    if autostart {
        start_instance(st, &name).await?;
    }
    Ok(format!("实例 {name} 已创建"))
}

/// 扫描配置目录装载插件/实例/定时任务；reload=true 时做增量同步
pub async fn load_configs(st: &Arc<AppState>, reload: bool) -> Result<String> {
    let plugin_list = crate::config::load_plugins(&st.root_dir.join("plugins"))?;
    {
        let mut plugins = st.plugins.write().await;
        plugins.clear();
        for p in plugin_list {
            plugins.insert(p.name.clone(), p);
        }
    }

    let specs = crate::config::load_instances(&st.root_dir.join("instances"))?;
    let mut report: Vec<String> = Vec::new();
    let mut autostarts: Vec<String> = Vec::new();
    {
        let mut map = st.instances.write().await;
        let mut loaded_names: Vec<String> = Vec::new();
        for spec in specs {
            let name = spec.name.clone();
            loaded_names.push(name.clone());
            if map.contains_key(&name) {
                if !reload {
                    report.push(format!("{name}: 已存在，跳过"));
                }
                continue;
            }
            if spec.autostart {
                autostarts.push(name.clone());
            }
            map.insert(name.clone(), Instance::new(spec));
            report.push(format!("{name}: 已装载"));
        }
        if reload {
            let existing: Vec<String> = map.keys().cloned().collect();
            for n in existing {
                if !loaded_names.contains(&n) {
                    if let Some(inst) = map.get(&n) {
                        if inst.status().await.alive() {
                            inst.desired.store(false, Ordering::SeqCst);
                            if let Some(tx) = inst.ctrl.read().await.clone() {
                                let _ = tx.send(Ctrl::Stop).await;
                            }
                        }
                    }
                    map.remove(&n);
                    report.push(format!("{n}: 已移除"));
                }
            }
        }
    }

    let scheds = crate::config::load_schedules(&st.root_dir.join("schedules"))?;
    *st.schedules.write().await = scheds;

    for n in autostarts {
        let st2 = st.clone();
        tokio::spawn(async move {
            if let Err(e) = start_instance(&st2, &n).await {
                tracing::warn!("自动启动 {n} 失败: {e:#}");
            }
        });
    }

    Ok(report.join("\n"))
}

// ---------------------------------------------------------------------------
// 生命周期守护：单个任务内完成 spawn → 监控 → 崩溃重启 循环
// ---------------------------------------------------------------------------

fn build_command(launch: &ResolvedLaunch) -> Result<std::process::Command> {
    let mut cmd = std::process::Command::new(&launch.program);
    cmd.args(&launch.args);
    for (k, v) in &launch.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &launch.cwd {
        if !dir.exists() {
            bail!("工作目录不存在: {}", dir.display());
        }
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：避免每个子进程弹出控制台
        cmd.creation_flags(0x0800_0000);
    }
    Ok(cmd)
}

async fn run_lifecycle(st: Arc<AppState>, inst: Arc<Instance>, launch: ResolvedLaunch) {
    let _guard = inst.start_lock.lock().await;
    let name = inst.spec.name.clone();

    loop {
        inst.restarts.fetch_add(1, Ordering::SeqCst);

        // --- 创建管道 ---
        let (stdout_r, stdout_stdio) = match pipes::output_pipe() {
            Ok(p) => p,
            Err(e) => {
                fail_spawn(&st, &inst, format!("创建管道失败: {e}")).await;
                return;
            }
        };
        let (stderr_r, stderr_stdio) = match pipes::output_pipe() {
            Ok(p) => p,
            Err(e) => {
                fail_spawn(&st, &inst, format!("创建管道失败: {e}")).await;
                return;
            }
        };
        let (stdin_w, stdin_stdio) = match pipes::input_pipe() {
            Ok(p) => p,
            Err(e) => {
                fail_spawn(&st, &inst, format!("创建管道失败: {e}")).await;
                return;
            }
        };

        // --- 拉起子进程 ---
        let mut cmd = match build_command(&launch) {
            Ok(c) => c,
            Err(e) => {
                fail_spawn(&st, &inst, format!("{e}")).await;
                return;
            }
        };
        cmd.stdout(stdout_stdio).stderr(stderr_stdio).stdin(stdin_stdio);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                fail_spawn(&st, &inst, format!("启动 {} 失败: {e}", launch.program)).await;
                return;
            }
        };
        let pid = child.id();
        let child_shared: SharedChild = Arc::new(Mutex::new(Some(child)));
        let stdin_shared: SharedStdin = Arc::new(Mutex::new(Some(stdin_w)));

        inst.pid.store(pid as i32, Ordering::SeqCst);
        inst.set_status(RunStatus::Running).await;
        *inst.started_at.write().await = Some(Local::now());
        crate::events::emit(&st, "instance.start", &name, "info", &format!("子进程已拉起 pid={pid} port={:?}", inst.spec.port)).await;

        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Ctrl>(64);
        *inst.ctrl.write().await = Some(ctrl_tx);

        // 日志聚合任务：阻塞线程 → channel → push_log（落盘 + 内存 + 广播）
        let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogLine>();
        {
            let st2 = st.clone();
            let inst2 = inst.clone();
            let log_path = st.log_file_path(&name);
            tokio::spawn(async move {
                let file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .await
                    .ok();
                let wf = Arc::new(AsyncMutex::new(file));
                while let Some(line) = log_rx.recv().await {
                    inst2.push_log(st2.cfg.node.log_buffer_lines, line, &wf).await;
                }
            });
        }

        let exited = Arc::new(AtomicBool::new(false));
        spawn_reader(stdout_r, "out", log_tx.clone(), exited.clone());
        spawn_reader(stderr_r, "err", log_tx.clone(), exited.clone());
        drop(log_tx);

        // 健康检查
        if let Some(url) = launch.health_url.clone() {
            let st2 = st.clone();
            let inst2 = inst.clone();
            tokio::spawn(health_loop(st2, inst2, url, launch.health_interval_secs));
        }

        // 退出等待线程：轮询 try_wait
        let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel::<String>();
        {
            let cs = child_shared.clone();
            std::thread::spawn(move || loop {
                let status = {
                    let mut g = match cs.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    match g.as_mut() {
                        Some(c) => match c.try_wait() {
                            Ok(Some(s)) => Some(match s.code() {
                                Some(code) => format!("exit code {code}"),
                                None => format!("{s}"),
                            }),
                            Ok(None) => None,
                            Err(e) => Some(format!("wait error: {e}")),
                        },
                        None => return,
                    }
                };
                if let Some(desc) = status {
                    let _ = exit_tx.send(desc);
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            });
        }

        // --- 监控循环 ---
        let started = Instant::now();
        let mut stop_requested = false;
        let mut grace_deadline: Option<tokio::time::Instant> = None;
        let exit_desc;
        loop {
            tokio::select! {
                r = &mut exit_rx => {
                    exit_desc = r.unwrap_or_else(|_| "unknown".into());
                    break;
                }
                c = ctrl_rx.recv() => {
                    match c {
                        Some(Ctrl::Stdin(line)) => {
                            let stdin = stdin_shared.clone();
                            tokio::task::spawn_blocking(move || {
                                if let Ok(mut g) = stdin.lock() {
                                    if let Some(w) = g.as_mut() {
                                        let _ = w.write_all(format!("{line}\n").as_bytes());
                                        let _ = w.flush();
                                    }
                                }
                            });
                        }
                        Some(Ctrl::Stop) => {
                            stop_requested = true;
                            if let Some(line) = &launch.shutdown_line {
                                let stdin = stdin_shared.clone();
                                let line = line.clone();
                                tokio::task::spawn_blocking(move || {
                                    if let Ok(mut g) = stdin.lock() {
                                        if let Some(w) = g.as_mut() {
                                            let _ = w.write_all(format!("{line}\n").as_bytes());
                                            let _ = w.flush();
                                        }
                                    }
                                });
                            }
                            grace_deadline = Some(tokio::time::Instant::now()
                                + Duration::from_secs(launch.grace_secs));
                        }
                        None => {
                            // 通道关闭（异常情况）：防止孤儿进程
                            kill_child(&child_shared);
                        }
                    }
                }
                _ = async {
                    match grace_deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    kill_child(&child_shared);
                    grace_deadline = None;
                }
            }
        }

        // --- 退出收尾 ---
        exited.store(true, Ordering::SeqCst); // 通知读线程排空后退出
        *inst.ctrl.write().await = None;
        inst.pid.store(-1, Ordering::SeqCst);
        inst.set_last_exit(exit_desc.clone()).await;
        if stop_requested {
            inst.set_status(RunStatus::Stopped).await;
            crate::events::emit(&st, "instance.stop", &name, "info", "实例已停止").await;
            return;
        }

        let code_ok = exit_desc == "exit code 0";
        inst.set_status(if code_ok { RunStatus::Exited } else { RunStatus::Failed }).await;
        crate::events::emit(&st, "instance.exit", &name, "warn", &format!("异常退出: {exit_desc}")).await;
        if started.elapsed() > Duration::from_secs(120) {
            inst.restarts.store(0, Ordering::SeqCst);
        }

        let should = match inst.spec.restart_policy {
            crate::config::RestartPolicy::No => false,
            crate::config::RestartPolicy::OnFailure => !code_ok,
            crate::config::RestartPolicy::Always => true,
        };
        if !should {
            return;
        }
        let retries = inst.restarts.load(Ordering::SeqCst);
        if retries > inst.spec.max_retries as u64 {
            tracing::error!(
                "实例 {name} 达到最大重试次数({})，停止重启",
                inst.spec.max_retries
            );
            crate::events::emit(
                &st,
                "instance.giveup",
                &name,
                "error",
                &format!("连续失败 {} 次，放弃自动重启（最后退出: {exit_desc}）", inst.spec.max_retries),
            )
            .await;
            return;
        }
        let backoff = Duration::from_secs((2 * retries).clamp(1, 30));
        tracing::warn!(
            "实例 {name} 退出（{exit_desc}），{backoff:?} 后第 {retries} 次自动重启"
        );
        crate::events::emit(
            &st,
            "instance.restart",
            &name,
            "warn",
            &format!("第 {retries} 次自动重启（{backoff:?} 后）,上次退出: {exit_desc}"),
        )
        .await;
        tokio::time::sleep(backoff).await;
        if !inst.desired.load(Ordering::SeqCst) {
            return;
        }
        // 继续下一轮循环（重新拉起）
    }
}

async fn fail_spawn(st: &Arc<AppState>, inst: &Arc<Instance>, msg: String) {
    inst.set_status(RunStatus::Failed).await;
    inst.set_last_exit(msg.clone()).await;
    tracing::error!("实例 {} 启动失败: {msg}", inst.spec.name);
    crate::events::emit(st, "instance.fail", &inst.spec.name, "error", &format!("启动失败: {msg}")).await;
}

fn kill_child(child: &SharedChild) {
    if let Ok(mut g) = child.lock() {
        if let Some(c) = g.as_mut() {
            let _ = c.kill();
        }
    }
}

/// 输出泵线程：读取管道并按行发出。不依赖 EOF —— Windows 上轮询可读字节数，
/// 子进程退出（exited=true）后排空剩余数据即退出。
fn spawn_reader(
    mut r: pipes::PipeReader,
    stream: &'static str,
    tx: mpsc::UnboundedSender<LogLine>,
    exited: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let make_log = |text: String| LogLine {
            ts: Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            stream: stream.to_string(),
            text,
        };
        #[cfg(windows)]
        {
            let mut linebuf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match r.available() {
                    Ok(0) => {
                        if exited.load(Ordering::SeqCst) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(_) => {
                        // 读取当前可用的全部数据
                        loop {
                            match r.read(&mut chunk) {
                                Ok(0) => break,
                                Ok(n) => linebuf.extend_from_slice(&chunk[..n]),
                                Err(_) => break,
                            }
                            match r.available() {
                                Ok(n) if n > 0 => continue,
                                _ => break,
                            }
                        }
                        // 按行拆分
                        while let Some(pos) = linebuf.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = linebuf.drain(..=pos).collect();
                            let text = String::from_utf8_lossy(&line[..pos])
                                .trim_end_matches('\r')
                                .to_string();
                            let _ = tx.send(make_log(text.chars().take(4000).collect()));
                        }
                    }
                    Err(_) => return, // 管道已断
                }
            }
            // 收尾：残留未换行的内容
            if !linebuf.is_empty() {
                let text = String::from_utf8_lossy(&linebuf).to_string();
                let _ = tx.send(make_log(text.chars().take(4000).collect()));
            }
        }
        #[cfg(not(windows))]
        {
            use std::io::BufRead;
            let mut lines = std::io::BufReader::new(r).lines();
            while let Some(Ok(line)) = lines.next() {
                let _ = tx.send(make_log(line.chars().take(4000).collect()));
            }
        }
    });
}

async fn health_loop(st: Arc<AppState>, inst: Arc<Instance>, url: String, interval: u64) {
    let mut misses: u32 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if !inst.desired.load(Ordering::SeqCst) {
            break;
        }
        let cur = inst.status().await;
        if !matches!(cur, RunStatus::Running | RunStatus::Unhealthy) {
            break;
        }
        let ok = st
            .local_client
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            misses = 0;
            if cur == RunStatus::Unhealthy {
                inst.set_status(RunStatus::Running).await;
            }
        } else {
            misses += 1;
            if misses >= 3 && cur == RunStatus::Running {
                tracing::warn!("实例 {} 健康检查连续失败，标记 unhealthy", inst.spec.name);
                inst.set_status(RunStatus::Unhealthy).await;
                crate::events::emit(&st, "instance.unhealthy", &inst.spec.name, "warn", "健康检查连续 3 次失败").await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// exec：节点上执行 shell 命令（供 API 调用；在阻塞线程中运行）
// ---------------------------------------------------------------------------

pub struct ExecOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// 阻塞执行 shell 命令（请在 tokio::task::spawn_blocking 中调用）
pub fn exec_command_blocking(command: &str, timeout: Duration) -> ExecOutput {
    let (out_r, out_c) = match pipes::output_pipe() {
        Ok(p) => p,
        Err(e) => return ExecOutput { code: None, stdout: String::new(), stderr: format!("pipe error: {e}"), timed_out: false },
    };
    let (err_r, err_c) = match pipes::output_pipe() {
        Ok(p) => p,
        Err(e) => return ExecOutput { code: None, stdout: String::new(), stderr: format!("pipe error: {e}"), timed_out: false },
    };
    let (_in_w, in_c) = match pipes::input_pipe() {
        Ok(p) => p,
        Err(e) => return ExecOutput { code: None, stdout: String::new(), stderr: format!("pipe error: {e}"), timed_out: false },
    };

    let mut cmd = std::process::Command::new(if cfg!(windows) { "cmd" } else { "bash" });
    if cfg!(windows) {
        cmd.args(["/C", command]);
    } else {
        cmd.args(["-lc", command]);
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.stdout(out_c).stderr(err_c).stdin(in_c);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ExecOutput { code: None, stdout: String::new(), stderr: format!("执行失败: {e}"), timed_out: false },
    };

    // 读线程（排空管道避免写满死锁；不依赖 EOF）
    let exited = Arc::new(AtomicBool::new(false));
    let t1_out = spawn_collect(out_r, exited.clone(), 200 * 1024);
    let t2_err = spawn_collect(err_r, exited.clone(), 200 * 1024);

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };
    exited.store(true, Ordering::SeqCst);
    let code = status.and_then(|s| s.code());
    let stdout = t1_out.join().unwrap_or_default();
    let stderr = t2_err.join().unwrap_or_default();
    ExecOutput { code, stdout, stderr, timed_out }
}

fn spawn_collect(
    mut r: pipes::PipeReader,
    exited: Arc<AtomicBool>,
    cap: usize,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        #[cfg(windows)]
        {
            loop {
                match r.available() {
                    Ok(0) => {
                        if exited.load(Ordering::SeqCst) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(_) => {
                        loop {
                            match r.read(&mut chunk) {
                                Ok(0) => break,
                                Ok(n) => {
                                    let take = (cap - buf.len()).min(n);
                                    buf.extend_from_slice(&chunk[..take]);
                                    if buf.len() >= cap {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                            match r.available() {
                                Ok(n) if n > 0 && buf.len() < cap => continue,
                                _ => break,
                            }
                        }
                        if buf.len() >= cap {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = &exited;
            loop {
                match r.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let take = (cap - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..take]);
                        if buf.len() >= cap {
                            break;
                        }
                    }
                }
            }
        }
        let mut s = String::from_utf8_lossy(&buf).to_string();
        if s.len() >= cap {
            s.push_str("...[截断]");
        }
        s
    })
}
