//! 工具永远不能读、写、搜索的目录。
//!
//! 这里刻意不复用 `observe::SKIP_DIRS`：那张表的语义是「扫描工作区做变更
//! 观测时不想看的大目录」（`node_modules`、`target`……），按噪音/性能调整；
//! 这张表是安全边界，按「能否交给模型」调整。两者变化的方向不同，合并会
//! 让其中一张表为了另一张表的目的被改宽。
//!
//! 单一来源的消费方：`fs_search` 的 walk、`grep`（系统 grep 的
//! `--exclude-dir` 与无 grep 时的 `literal_search`）、`tree`，以及
//! `Policy::defaults` 里针对这些目录的 Deny 规则。

use std::path::{Component, Path};

/// 受保护目录名。大小写不敏感，因为 Windows 文件系统不区分大小写：
/// `Secrets` 与 `secrets` 是同一个目录，按精确字节比较会漏。
pub const PROTECTED_DIRS: &[&str] = &["secrets"];

/// 目录名是否受保护。
pub fn is_protected_dir(name: &str) -> bool {
    PROTECTED_DIRS
        .iter()
        .any(|dir| name.eq_ignore_ascii_case(dir))
}

/// `path` 中是否有任何一段（相对 workspace）是受保护目录。
///
/// 先按路径分量剥掉 workspace，避免 workspace 自己的路径里恰好含有
/// `secrets` 时把整个工作区误判成受保护。
pub fn is_protected_path(workspace: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(workspace).unwrap_or(path);
    rel.components().any(|c| match c {
        Component::Normal(name) => name.to_str().map(is_protected_dir).unwrap_or(false),
        _ => false,
    })
}

/// 解析真实路径后再判断一次，挡住「工作区里的符号链接别名指向 secrets」。
///
/// 目标不存在（例如写新文件）时返回 `false`；写目标由策略层和
/// `agent::escapes_workspace` 负责。
pub fn is_protected_real_path(workspace: &Path, path: &Path) -> bool {
    let real = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    is_protected_path(&ws, &real)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_names_are_case_insensitive() {
        assert!(is_protected_dir("secrets"));
        assert!(is_protected_dir("Secrets"));
        assert!(is_protected_dir("SECRETS"));
        assert!(!is_protected_dir("secret"));
        assert!(!is_protected_dir("src"));
    }

    #[test]
    fn protected_path_only_looks_below_the_workspace() {
        let ws = Path::new("/home/me/secrets-project");
        assert!(is_protected_path(ws, &ws.join("secrets")));
        assert!(is_protected_path(ws, &ws.join("secrets/astra.json")));
        assert!(is_protected_path(ws, &ws.join("src/secrets/x")));
        assert!(!is_protected_path(ws, &ws.join("src/main.rs")));
        assert!(!is_protected_path(ws, ws));
    }
}
