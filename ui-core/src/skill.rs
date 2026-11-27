//! Skill：按需加载的指令包。
//!
//! 渐进披露：提示里只放**目录**（名字 + 一句话），模型觉得需要时用 `skill` 工具
//! 把正文拉进上下文。这样几十个 skill 也只占几行，而不是几万 token。
//!
//! 加载过的正文会作为独立段落进入快照（可计价），并写一条 `skill/load` 事件。

use crate::protected::is_sensitive_or_private;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub skills: Vec<Skill>,
}

impl Catalog {
    /// 在 `<ws>/skills` 下找 `*/SKILL.md`。
    ///
    /// `.ui` 是受保护目录，不再作为隐式 skill 根；否则模型只要命名一个
    /// `skill` 就能绕过 `fs_read` 的 `.ui` 边界。目录与文件都过统一隐私
    /// 判定，且真实路径必须仍位于 canonicalize 后的 `skills/` 内，软链不能
    /// 把读取偷渡到工作区外或 `secrets/`。
    pub fn discover(workspace: &Path) -> Catalog {
        let mut skills: Vec<Skill> = Vec::new();
        let ws_real = match workspace.canonicalize() {
            Ok(p) => p,
            Err(_) => return Catalog { skills },
        };
        let root = ws_real.join("skills");
        let root_real = match root.canonicalize() {
            Ok(p) => p,
            Err(_) => return Catalog { skills },
        };
        if !root_real.starts_with(&ws_real) || is_sensitive_or_private(&ws_real, &root_real) {
            return Catalog { skills };
        }
        let entries = match fs::read_dir(&root_real) {
            Ok(e) => e,
            Err(_) => return Catalog { skills },
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if is_sensitive_or_private(&ws_real, &dir) {
                continue;
            }
            let file = dir.join("SKILL.md");
            let real_file = match file.canonicalize() {
                Ok(p) if p.starts_with(&root_real) => p,
                _ => continue,
            };
            if is_sensitive_or_private(&ws_real, &real_file) || !real_file.is_file() {
                continue;
            }
            if let Ok(text) = fs::read_to_string(&real_file) {
                if let Some(skill) = parse(&text, &real_file) {
                    if !skills.iter().any(|s| s.name == skill.name) {
                        skills.push(skill);
                    }
                }
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Catalog { skills }
    }

    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// 给提示用的目录：一行一个，只有名字和描述。
    pub fn render_catalog(&self) -> String {
        self.skills
            .iter()
            .map(|s| format!("- {} — {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `---\nname: x\ndescription: y\n---\n正文`。缺 frontmatter 时用目录名当名字。
pub fn parse(text: &str, path: &Path) -> Option<Skill> {
    let mut name = String::new();
    let mut description = String::new();
    let mut body = text.to_string();

    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            for line in front.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("name:") {
                    name = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = v.trim().trim_matches('"').to_string();
                }
            }
            body = rest[end + 4..].trim_start_matches('\n').to_string();
        }
    }

    if name.is_empty() {
        name = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_string());
    }
    if description.is_empty() {
        description = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim().chars().take(60).collect())
            .unwrap_or_default();
    }

    Some(Skill {
        name,
        description,
        body,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ui-skill-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(ws: &Path, dir: &str, text: &str) {
        let d = ws.join("skills").join(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), text).unwrap();
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let p = Path::new("/x/SKILL.md");
        let s = parse("---\nname: pdf\ndescription: 处理 PDF\n---\n# 用法\n先装依赖。", p).unwrap();
        assert_eq!(s.name, "pdf");
        assert_eq!(s.description, "处理 PDF");
        assert!(s.body.contains("先装依赖"));
    }

    #[test]
    fn falls_back_to_directory_name() {
        let p = Path::new("/skills/release-notes/SKILL.md");
        let s = parse("没有 frontmatter 的正文", p).unwrap();
        assert_eq!(s.name, "release-notes");
        assert_eq!(s.description, "没有 frontmatter 的正文");
    }

    #[test]
    fn discovers_and_renders_catalog() {
        let ws = tmp("discover");
        write_skill(&ws, "alpha", "---\nname: alpha\ndescription: 第一件事\n---\n正文 A");
        write_skill(&ws, "beta", "---\nname: beta\ndescription: 第二件事\n---\n正文 B");
        let cat = Catalog::discover(&ws);
        assert_eq!(cat.skills.len(), 2);
        assert!(cat.find("alpha").is_some());
        let rendered = cat.render_catalog();
        assert!(rendered.contains("- alpha — 第一件事"));
        assert!(rendered.contains("- beta — 第二件事"));
    }

    #[test]
    fn discovery_ignores_ui_private_and_sensitive_entries() {
        let ws = tmp("discover-filtered");
        write_skill(&ws, "alpha", "---\nname: alpha\ndescription: 可见\n---\n正文 A");
        write_skill(&ws, "secrets/hidden", "---\nname: hidden\n---\n隐私目录");
        write_skill(&ws, ".env/hidden", "---\nname: env-hidden\n---\n敏感目录");

        let ui = ws.join(".ui").join("skills").join("hidden");
        fs::create_dir_all(&ui).unwrap();
        fs::write(ui.join("SKILL.md"), "---\nname: ui-hidden\n---\n受保护目录").unwrap();

        let cat = Catalog::discover(&ws);
        assert_eq!(cat.skills.len(), 1);
        assert_eq!(cat.skills[0].name, "alpha");
    }

    #[test]
    fn discovery_honors_custom_private_paths() {
        let _g = crate::protected::private_test_guard();
        crate::protected::clear_private();
        crate::protected::add_private("vendor/keys").unwrap();
        let ws = tmp("discover-custom-private");
        write_skill(&ws, "alpha", "---\nname: alpha\n---\n可见");
        write_skill(&ws, "vendor/keys/hidden", "---\nname: hidden\n---\n自定义隐私");

        let cat = Catalog::discover(&ws);
        assert_eq!(cat.skills.len(), 1);
        assert_eq!(cat.skills[0].name, "alpha");
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let ws = tmp("discover-symlink");
        write_skill(&ws, "alpha", "---\nname: alpha\n---\n可见");
        let secrets = ws.join("secrets");
        fs::create_dir_all(secrets.join("evil")).unwrap();
        fs::write(secrets.join("evil").join("SKILL.md"), "---\nname: evil\n---\n越界").unwrap();
        symlink(&secrets, ws.join("skills").join("alias")).unwrap();

        let beta = ws.join("skills").join("beta");
        fs::create_dir_all(&beta).unwrap();
        symlink(secrets.join("evil").join("SKILL.md"), beta.join("SKILL.md")).unwrap();

        let cat = Catalog::discover(&ws);
        assert_eq!(cat.skills.len(), 1);
        assert_eq!(cat.skills[0].name, "alpha");
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let ws = tmp("empty");
        let cat = Catalog::discover(&ws);
        assert!(cat.is_empty());
        assert_eq!(cat.render_catalog(), "");
    }
}
