// 把 commit 与构建日期编进二进制：没有第三方依赖，只用 std + 外部命令。
//
// 目标：任何人拿到 `ui` 都能回答"这是哪个构建"。
use std::process::Command;

fn out(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    if !o.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn main() {
    // 构建发生在副本目录（没有 .git），所以从 UI_GIT_DIR 指向的源码仓库取
    let git_dir = std::env::var("UI_GIT_DIR").ok();
    let git = |args: &[&str]| -> Option<String> {
        match &git_dir {
            Some(dir) => {
                let mut full = vec!["-C", dir.as_str()];
                full.extend_from_slice(args);
                out("git", &full)
            }
            None => out("git", args),
        }
    };

    // commit：有 git 就取短 sha，没有就标 unknown（源码包场景）
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    // 是否在脏工作区构建：这比 commit 更能说明"测的是哪份代码"
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let commit = if dirty { format!("{}-dirty", commit) } else { commit };
    println!("cargo:rustc-env=UI_COMMIT={}", commit);

    // 构建日期：优先 SOURCE_DATE_EPOCH（可复现构建），否则取当前 UTC 日期
    let date = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(epoch) => format!("epoch:{}", epoch),
        Err(_) => out("date", &["-u", "+%Y-%m-%d"]).unwrap_or_else(|| "unknown".to_string()),
    };
    println!("cargo:rustc-env=UI_BUILD_DATE={}", date);

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
