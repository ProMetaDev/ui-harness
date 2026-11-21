//! 子进程执行层的公共部分：环境清理与 Linux 文件系统沙箱。
//!
//! shell、plugin、MCP、grep 各自有 transport/协议差异，但“继承什么环境”
//! 和“能不能写工作区外”必须只有一份实现。否则修了 shell 的 denylist，
//! plugin/MCP 仍是旁路。

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 单个子进程输出在读取阶段的硬上限（8 MiB）。
pub const MAX_CHILD_CAPTURE: u64 = 8 * 1024 * 1024;

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

#[cfg(unix)]
extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// 让子进程自成进程组，超时/取消时能一次收掉后代。
pub fn isolate_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// 杀掉整棵进程树。Unix 用负 pid 杀进程组；Windows 用 taskkill /T。
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        kill(-(pid as i32), 9);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

pub struct Captured {
    pub status: Option<ExitStatus>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

fn read_capped<R: Read>(reader: R, max: u64) -> String {
    let mut buf = Vec::new();
    let mut limited = reader.take(max + 1);
    let _ = limited.read_to_end(&mut buf);
    let truncated = buf.len() as u64 > max;
    if truncated {
        buf.truncate(max as usize);
    }
    let mut s = String::from_utf8_lossy(&buf).to_string();
    if truncated {
        s.push_str(&format!("\n…[输出超过 {} MiB，已截断]", max / 1024 / 1024));
    }
    s
}

/// 等待一个已 spawn 的子进程，带超时与有界 stdout/stderr。
///
/// `Child` 的 stdin 由调用方负责（plugin 要先写 payload）。这里不 close
/// 父进程的 stdout 读端：即使超时，reader 线程最多再等 2 秒，不会把整个
/// turn 卡死；进程树清理由各平台调用方负责。
pub fn wait_capped(mut child: Child, timeout: Duration, max: u64) -> Captured {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    if let Some(stdout) = stdout {
        std::thread::spawn(move || {
            let _ = out_tx.send(read_capped(stdout, max));
        });
    }
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let _ = err_tx.send(read_capped(stderr, max));
        });
    }

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(15));
            }
            Ok(None) => {
                timed_out = true;
                let _ = child.kill();
                kill_tree(child.id());
                break child.wait().ok();
            }
            Err(_) => break None,
        }
    };
    let stdout = out_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    let stderr = err_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    Captured {
        status,
        stdout,
        stderr,
        timed_out,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_userinfo_is_scrubbed_but_host_is_kept() {
        let got = scrub_url_userinfo(OsStr::new("postgres://user:secret@db:5432/app"));
        assert_eq!(got, OsString::from("postgres://***@db:5432/app"));
    }

    #[test]
    fn known_credential_channels_are_sensitive_even_without_key_in_name() {
        assert!(sensitive_env_name("SSH_AUTH_SOCK"));
        assert!(sensitive_env_name("KUBECONFIG"));
        assert!(!sensitive_env_name("PATH"));
    }

    #[cfg(unix)]
    #[test]
    fn wait_capped_times_out_and_kills_the_process_group() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5");
        isolate_group(&mut cmd);
        let child = cmd.spawn().unwrap();
        let pid = child.id();
        let out = wait_capped(child, Duration::from_millis(100), 1024);
        assert!(out.timed_out, "sleep 5 must time out");
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !std::path::Path::new(&format!("/proc/{}", pid)).exists(),
            "timed out child {} must be reaped",
            pid
        );
    }
}
