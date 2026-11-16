//! Skill：按需加载的指令包。
//!
//! 渐进披露：提示里只放**目录**（名字 + 一句话），模型觉得需要时用 `skill` 工具
//! 把正文拉进上下文。这样几十个 skill 也只占几行，而不是几万 token。
//!
//! 加载过的正文会作为独立段落进入快照（可计价），并写一条 `skill/load` 事件。

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
    /// 在 `<ws>/skills` 与 `<ws>/.ui/skills` 下找 `*/SKILL.md`。
    pub fn discover(workspace: &Path) -> Catalog {
        let mut roots = vec![workspace.join("skills"), workspace.join(".ui").join("skills")];
        roots.dedup();
        let mut skills: Vec<Skill> = Vec::new();
        for root in roots {
            let entries = match fs::read_dir(&root) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let dir = entry.path();
                let file = dir.join("SKILL.md");
                if !file.is_file() {
                    continue;
                }
                if let Ok(text) = fs::read_to_string(&file) {
                    if let Some(skill) = parse(&text, &file) {
                        if !skills.iter().any(|s| s.name == skill.name) {
                            skills.push(skill);
                        }
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
    fn missing_directory_is_not_an_error() {
        let ws = tmp("empty");
        let cat = Catalog::discover(&ws);
        assert!(cat.is_empty());
        assert_eq!(cat.render_catalog(), "");
    }
}
