use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::config::{resolve_path, InstanceSpec, PluginSpec};

/// 插件模板与实例规格合并后的启动方案
#[derive(Debug, Clone)]
pub struct ResolvedLaunch {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub health_url: Option<String>,
    pub health_interval_secs: u64,
    pub grace_secs: u64,
    pub shutdown_line: Option<String>,
}

fn subst(tpl: &str, script: &str, port: &str, name: &str) -> String {
    tpl.replace("{script}", script)
        .replace("{port}", port)
        .replace("{name}", name)
}

/// 将插件模板与实例规格合并为可执行方案
pub fn resolve(
    root: &PathBuf,
    plugins: &HashMap<String, PluginSpec>,
    spec: &InstanceSpec,
) -> Result<ResolvedLaunch> {
    let plugin = plugins
        .get(&spec.plugin)
        .ok_or_else(|| anyhow!("未找到插件 \"{}\"，请检查 plugins/ 目录", spec.plugin))?;

    let script = spec.script.clone().unwrap_or_default();
    let port = spec.port.map(|p| p.to_string()).unwrap_or_else(|| "0".into());
    let name = spec.name.as_str();

    let program = subst(&plugin.command, &script, &port, name);
    if program.trim().is_empty() {
        return Err(anyhow!("插件 {} 的 command 为空", spec.plugin));
    }

    let mut args: Vec<String> = plugin
        .args
        .iter()
        .map(|a| subst(a, &script, &port, name))
        .collect();
    for a in &spec.args {
        args.push(subst(a, &script, &port, name));
    }

    let mut env = plugin.env.clone();
    for (k, v) in &spec.env {
        env.insert(k.clone(), subst(v, &script, &port, name));
    }
    if let Some(p) = spec.port {
        env.insert("PORT".into(), p.to_string());
    }

    let health_url = spec
        .health_url
        .clone()
        .or_else(|| plugin.health_url.clone())
        .map(|h| subst(&h, &script, &port, name));

    let cwd = resolve_path(root, &spec.workdir);

    Ok(ResolvedLaunch {
        program,
        args,
        env,
        cwd,
        health_url,
        health_interval_secs: plugin.health_interval_secs.max(3),
        grace_secs: plugin.grace_secs.max(1),
        shutdown_line: spec.shutdown_line.clone().or_else(|| plugin.shutdown_line.clone()),
    })
}
