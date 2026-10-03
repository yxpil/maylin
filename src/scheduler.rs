use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use cron::Schedule;

use crate::config::ScheduleSpec;
use crate::process;
use crate::state::AppState;

/// 为每个启用的定时任务启动独立循环
pub async fn spawn_all(st: Arc<AppState>) {
    let scheds = st.schedules.read().await.clone();
    for s in scheds {
        if !s.enabled {
            continue;
        }
        let st2 = st.clone();
        tokio::spawn(schedule_loop(st2, s));
    }
}

async fn schedule_loop(st: Arc<AppState>, s: ScheduleSpec) {
    // cron crate 需要 6 段（含秒），用户写标准 5 段，前面补 "0 "
    let expr = if s.cron.split_whitespace().count() == 5 {
        format!("0 {}", s.cron)
    } else {
        s.cron.clone()
    };
    let sched = match Schedule::from_str(&expr) {
        Ok(sc) => sc,
        Err(e) => {
            tracing::error!("定时任务 {} cron 表达式无效({}): {e}", s.name, expr);
            return;
        }
    };
    tracing::info!(
        "定时任务 [{}] 已启用: {} {} {}",
        s.name,
        s.cron,
        s.action,
        s.target
    );
    loop {
        let now = Local::now();
        let next = match sched.upcoming(Local).next() {
            Some(t) => t,
            None => return,
        };
        let wait = (next - now).to_std().unwrap_or(Duration::from_secs(1));
        tokio::time::sleep(wait).await;
        // 睡醒后时钟回拨等异常保护
        if Local::now() < next - chrono::TimeDelta::try_seconds(1).unwrap() {
            continue;
        }
        tracing::info!("定时任务 [{}] 触发: {} {}", s.name, s.action, s.target);
        crate::events::emit(
            &st,
            "schedule.fire",
            &s.name,
            "info",
            &format!("触发 {} {}", s.action, s.target),
        )
        .await;
        run_action(&st, &s).await;
    }
}

async fn run_action(st: &Arc<AppState>, s: &ScheduleSpec) {
    let targets: Vec<String> = if s.target == "all" {
        st.instance_names().await
    } else {
        vec![s.target.clone()]
    };
    for t in targets {
        let r = match s.action.as_str() {
            "start" => process::start_instance(st, &t).await,
            "stop" => process::stop_instance(st, &t).await,
            "restart" => process::restart_instance(st, &t).await,
            other => Err(anyhow::anyhow!("未知动作 {other}")),
        };
        match r {
            Ok(msg) => tracing::info!("定时任务 [{}] {t}: {msg}", s.name),
            Err(e) => tracing::warn!("定时任务 [{}] {t} 失败: {e:#}", s.name),
        }
    }
}
