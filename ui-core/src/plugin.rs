//! 插件：一个进程 + 一份能力声明。
//!
//! 刻意不做「热加载任意代码」：每个插件必须在配置里**声明它能产生的效果**，
//! 没声明就加载失败。这样策略和审计照常工作，插件不会成为后门。
//!
//! 协议：插件从 stdin 收一行 JSON `{plugin,tool,args}`，往 stdout 写文本结果。

use std::path::Path;

use crate::json::{self, Json};
use crate::policy::Effect;

#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub summary: String,
    /// 声明的效果集（策略与日志都用它）
    pub effects: Vec<Effect>,
}

#[derive(Clone)]
pub struct Plugin {
    pub name: String,
    pub cmd: String,
    pub tools: Vec<ToolSpec>,
}

pub struct Set {
    pub plugins: Vec<Plugin>,
}

/// 已知的效果类型：写错一个字母就应该加载失败，而不是悄悄变成 allow。
const KNOWN_KINDS: &[&str] = &[
    "fs.read",
    "fs.write",
    "proc.exec",
    "net.fetch",
    "secret.read",
];

impl Set {
    pub fn load(path: &Path) -> Result<Set, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读不到插件配置 {}: {}", path.display(), e))?;
        let doc = json::parse(&text).map_err(|e| format!("插件配置不是 JSON: {}", e))?;
        let mut plugins = Vec::new();
        for raw in doc.arr_at("plugins").cloned().unwrap_or_default() {
            let name = raw.str_at("name").unwrap_or("").to_string();
            if name.is_empty() {
                return Err("插件缺少 name".to_string());
            }
            let cmd = raw.str_at("cmd").unwrap_or("").to_string();
            if cmd.is_empty() {
                return Err(format!("插件 {} 缺少 cmd", name));
            }
            let mut tools = Vec::new();
            for t in raw.arr_at("tools").cloned().unwrap_or_default() {
                let tname = t.str_at("name").unwrap_or("").to_string();
                if tname.is_empty() {
                    return Err(format!("插件 {} 有一个工具缺少 name", name));
                }
                // effects 必须显式出现（哪怕是空数组）——「没写」和「没有副作用」要能区分
                let declared = match t.get("effects") {
                    Some(Json::Arr(items)) => items.clone(),
                    Some(_) => return Err(format!("插件 {} 工具 {} 的 effects 必须是数组", name, tname)),
                    None => {
                        return Err(format!(
                            "插件 {} 工具 {} 没有声明 effects（没有副作用也要写 []）",
                            name, tname
                        ))
                    }
                };
                let mut effects = Vec::new();
                for item in declared {
                    let s = item.as_str().unwrap_or("");
                    let mut parts = s.splitn(2, ' ');
                    let kind = parts.next().unwrap_or("");
                    let target = parts.next().unwrap_or("").trim();
                    if !KNOWN_KINDS.contains(&kind) {
                        return Err(format!(
                            "插件 {} 工具 {} 声明了未知效果 `{}`（允许：{}）",
                            name,
                            tname,
                            kind,
                            KNOWN_KINDS.join(", ")
                        ));
                    }
                    effects.push(Effect::new(kind, target));
                }
                tools.push(ToolSpec {
                    name: tname,
                    summary: t.str_at("summary").unwrap_or("").to_string(),
                    effects,
                });
            }
            if tools.is_empty() {
                return Err(format!("插件 {} 没有声明任何工具", name));
            }
            plugins.push(Plugin { name, cmd, tools });
        }
        if plugins.is_empty() {
            return Err("插件配置里没有 plugins".to_string());
        }
        Ok(Set { plugins })
    }

    pub fn find(&self, plugin: &str, tool: &str) -> Option<&ToolSpec> {
        self.plugins
            .iter()
            .find(|p| p.name == plugin)
            .and_then(|p| p.tools.iter().find(|t| t.name == tool))
    }

    /// 插件声明的效果。相对目标按工作区解析——声明里写 `fs.write out.txt`
    /// 指的是工作区里的 out.txt，而不是字面路径 `out.txt`。
    pub fn effects_for(&self, plugin: &str, tool: &str, workspace: &Path) -> Vec<Effect> {
        let resolve = |target: &str| -> String {
            if target.is_empty() || target.starts_with('/') {
                target.to_string()
            } else {
                workspace.join(target).to_string_lossy().to_string()
            }
        };
        // 插件声明的效果只能“加严”，不能替代执行事实。任何插件调用都至少是
        // 一次 proc.exec：否则声明 `effects: []` 就能绕过默认的 proc.exec → Ask。
        let mut effects = vec![Effect::new(
            "proc.exec",
            format!("plugin:{}:{}", plugin, tool),
        )];
        if let Some(spec) = self.find(plugin, tool) {
            effects.extend(
                spec.effects
                    .iter()
                    .map(|e| Effect::new(&e.kind, resolve(&e.target))),
            );
        }
        effects
    }

    /// 给提示词用的清单
    pub fn catalog(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for p in &self.plugins {
            for t in &p.tools {
                out.push((
                    format!("plugin:{}:{}", p.name, t.name),
                    t.summary.clone(),
                ));
            }
        }
        out
    }

    pub fn call(
        &self,
        plugin: &str,
        tool: &str,
        args: &Json,
        scrub_env: bool,
        workspace: &Path,
        sandbox: &crate::sandbox::Config,
    ) -> Result<String, String> {
        let p = self
            .plugins
            .iter()
            .find(|p| p.name == plugin)
            .ok_or_else(|| format!("没有名为 {} 的插件", plugin))?;
        if !p.tools.iter().any(|t| t.name == tool) {
            return Err(format!("插件 {} 没有工具 {}", plugin, tool));
        }
        let payload = Json::obj(vec![
            ("plugin", Json::from(plugin)),
            ("tool", Json::from(tool)),
            ("args", args.clone()),
        ]);
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&p.cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if scrub_env {
            crate::exec::scrub_env(&mut cmd);
        }
        // 插件是本地进程，和 shell 一样必须有沙箱；统一走 exec 层。
        crate::exec::apply_linux_sandbox(&mut cmd, workspace, sandbox)?;
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动插件 {} 失败: {}", plugin, e))?;
        {
            let mut stdin = child.stdin.take().ok_or("拿不到插件 stdin")?;
            stdin
                .write_all(payload.to_compact().as_bytes())
                .map_err(|e| format!("写插件失败: {}", e))?;
        }
        let out = crate::exec::wait_capped(
            child,
            std::time::Duration::from_secs(30),
            8 * 1024 * 1024,
        );
        if out.timed_out {
            return Err(format!("插件 {} 超时（30s）", plugin));
        }
        let ok = out.status.as_ref().map(|s| s.success()).unwrap_or(false);
        if !ok {
            return Err(format!(
                "插件 {} 退出码 {:?}: {}",
                plugin,
                out.status.as_ref().and_then(|s| s.code()),
                out.stderr.trim()
            ));
        }
        Ok(out.stdout.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(effects: Vec<Effect>) -> Set {
        Set {
            plugins: vec![Plugin {
                name: "p".into(),
                cmd: "true".into(),
                tools: vec![ToolSpec {
                    name: "t".into(),
                    summary: String::new(),
                    effects,
                }],
            }],
        }
    }

    #[test]
    fn empty_declared_effects_still_produce_proc_exec() {
        let set = one(Vec::new());
        let effects = set.effects_for("p", "t", Path::new("/ws"));
        assert!(
            effects
                .iter()
                .any(|e| e.kind == "proc.exec" && e.target == "plugin:p:t"),
            "插件声明 [] 不能变成零效果：{:?}",
            effects
        );
    }

    #[test]
    fn declared_effects_are_additive_not_replacement() {
        let set = one(vec![Effect::new("fs.write", "out.txt")]);
        let effects = set.effects_for("p", "t", Path::new("/ws"));
        assert!(effects.iter().any(|e| e.kind == "proc.exec"));
        assert!(effects.iter().any(|e| e.kind == "fs.write"));
    }
}
