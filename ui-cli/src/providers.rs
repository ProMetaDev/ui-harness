//! Provider 注册表：把「用哪个模型」从硬编码变成一份可切换的配置。
//!
//! 设计取舍：适配器仍然是**外部进程**（`std::process`），内核只负责选谁、
//! 传什么模型名、把选择记进日志。这样加一个 provider 不需要改 Rust 代码，
//! 也不需要把 API key 交给内核。

use std::path::{Path, PathBuf};

use ui_core::json::{self, Json};

pub struct Provider {
    pub name: String,
    /// 启动适配器的命令（argv）
    pub cmd: Vec<String>,
    /// 传给适配器的模型名（UI_MODEL）
    pub model: String,
    /// 可选的键值环境（例如 base_url）
    pub env: Vec<(String, String)>,
}

pub struct Registry {
    pub default: String,
    pub providers: Vec<Provider>,
}

impl Registry {
    pub fn load(path: &Path) -> Result<Registry, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读不到 provider 配置 {}: {}", path.display(), e))?;
        let doc = json::parse(&text).map_err(|e| format!("provider 配置不是 JSON: {}", e))?;
        let default = doc.str_at("default").unwrap_or("").to_string();
        let mut providers = Vec::new();
        if let Some(map) = doc.get("providers").and_then(|v| match v {
            Json::Obj(items) => Some(items.clone()),
            _ => None,
        }) {
            for (name, spec) in map {
                // cmd 允许两种写法：字符串（整条 shell 命令）或 argv 数组
                let cmd: Vec<String> = match spec.get("cmd") {
                    Some(Json::Str(s)) => s.split_whitespace().map(str::to_string).collect(),
                    _ => spec
                        .arr_at("cmd")
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                };
                if cmd.is_empty() {
                    return Err(format!("provider {} 没有 cmd", name));
                }
                let model = spec.str_at("model").unwrap_or("").to_string();
                let mut env = Vec::new();
                if let Some(Json::Obj(items)) = spec.get("env").cloned() {
                    for (k, v) in items {
                        if let Some(s) = v.as_str() {
                            env.push((k, s.to_string()));
                        }
                    }
                }
                providers.push(Provider { name, cmd, model, env });
            }
        }
        if providers.is_empty() {
            return Err("provider 配置里没有 providers".to_string());
        }
        providers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Registry { default, providers })
    }

    pub fn get(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    /// 没指定就取 default；default 也没配就取第一个。
    pub fn pick(&self, want: Option<&str>) -> Option<&Provider> {
        match want {
            Some(n) => self.get(n),
            None => {
                if self.default.is_empty() {
                    self.providers.first()
                } else {
                    self.get(&self.default).or_else(|| self.providers.first())
                }
            }
        }
    }
}

/// 默认去哪里找配置：`<workspace>/.ui/providers.json`
pub fn default_path(workspace: &Path) -> PathBuf {
    workspace.join(".ui").join("providers.json")
}
