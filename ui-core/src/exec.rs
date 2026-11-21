//! 子进程执行层的公共部分：环境清理与 Linux 文件系统沙箱。
//!
//! shell、plugin、MCP、grep 各自有 transport/协议差异，但“继承什么环境”
//! 和“能不能写工作区外”必须只有一份实现。否则修了 shell 的 denylist，
//! plugin/MCP 仍是旁路。

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

/// 名字不像密钥、但实际是凭据通道的环境变量。
fn sensitive_env_name(name: &str) -> bool {
    matches!(
        name,
        "SSH_AUTH_SOCK"
            | "SSH_ASKPASS"
            | "KUBECONFIG"
            | "DOCKER_CONFIG"
            | "GIT_ASKPASS"
            | "NETRC"
    )
}

fn scrub_url_userinfo(value: &OsStr) -> OsString {
    let text = value.to_string_lossy();
    let Some(scheme_end) = text.find("://") else {
        return value.to_os_string();
    };
    let rest = &text[scheme_end + 3..];
    let Some(at) = rest.find('@') else {
        return value.to_os_string();
    };
    let authority = &rest[..at];
    if !authority.contains(':') {
        return value.to_os_string();
    }
    OsString::from(format!("{}://***@{}", &text[..scheme_end], &rest[at + 1..]))
}

/// 清掉密钥类环境变量，返回被剔除/改写的数量。
pub fn scrub_env(cmd: &mut Command) -> usize {
    let keep: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    cmd.env_clear();
    let mut scrubbed = 0usize;
    for (k, v) in keep {
        let key = k.to_string_lossy();
        if crate::secrets::key_is_secret(&key) || sensitive_env_name(&key) {
            scrubbed += 1;
            continue;
        }
        let value = scrub_url_userinfo(&v);
        if value != v {
            scrubbed += 1;
        }
        cmd.env(k, value);
    }
    scrubbed
}

/// Linux Landlock：在 exec 前挂上写限制。其他平台不假装有文件系统沙箱。
pub fn apply_linux_sandbox(
    cmd: &mut Command,
    workspace: &Path,
    cfg: &crate::sandbox::Config,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if cfg.fs {
            use std::os::unix::process::CommandExt;
            let ws = workspace.to_string_lossy().to_string();
            let hook = crate::sandbox::pre_exec(&ws, cfg)
                .map_err(|e| format!("沙箱不可用: {}", e))?;
            unsafe {
                cmd.pre_exec(hook);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cmd, workspace, cfg);
    }
    Ok(())
}
