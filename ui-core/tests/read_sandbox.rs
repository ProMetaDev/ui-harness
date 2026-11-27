#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use ui_core::{exec, protected, sandbox};

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ui-read-sandbox-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn run(workspace: &Path, script: &str) -> Output {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script).current_dir(workspace);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let cfg = sandbox::Config {
            read_roots: vec!["/usr".to_string(), "/etc".to_string()],
            ..Default::default()
        };
        exec::apply_linux_sandbox(&mut cmd, workspace, &cfg).unwrap();
        exec::isolate_group(&mut cmd);
        cmd.output().unwrap()
    }

    fn text(output: &Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    #[test]
    fn read_view_hides_private_paths_without_breaking_normal_work() {
        protected::set_privates(&["vendor/keys"]).unwrap();

        let base = temp_dir("case");
        let workspace = base.join("ws");
        let outside = base.join("outside");
        for dir in [
            "secrets",
            ".ui/tool-output",
            "nested/secrets",
            "vendor/keys",
            "src",
        ] {
            std::fs::create_dir_all(workspace.join(dir)).unwrap();
        }
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join("normal.txt"), "normal-ok").unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
        std::fs::write(workspace.join("secrets/token.txt"), "secret-value").unwrap();
        std::fs::write(workspace.join(".env"), "env-value").unwrap();
        std::fs::write(workspace.join(".ui/tool-output/cached.txt"), "cached-value").unwrap();
        std::fs::write(workspace.join("nested/secrets/token.txt"), "nested-secret").unwrap();
        std::fs::write(workspace.join("vendor/keys/token.txt"), "declared-secret").unwrap();
        std::fs::write(outside.join("outside.txt"), "outside-value").unwrap();
        std::os::unix::fs::symlink("secrets/token.txt", workspace.join("alias")).unwrap();

        let outside = outside.to_string_lossy();
        let script = format!(
            r#"
printf 'normal='; cat normal.txt
printf 'src='; cat src/lib.rs
printf 'system='; head -n 1 /etc/passwd >/dev/null && echo ok
for p in secrets/token.txt .env .ui/tool-output/cached.txt nested/secrets/token.txt vendor/keys/token.txt alias {outside}/outside.txt /proc/self/root/etc/passwd; do
  if cat "$p" >/dev/null 2>&1; then echo "LEAK:$p"; else echo "blocked:$p"; fi
done
printf 'created='; echo write-ok > created.txt && cat created.txt
command -v umount >/dev/null || {{ echo 'umount unavailable'; exit 97; }}
if mount --bind /etc/passwd /tmp/passwd-bind 2>/dev/null; then echo 'MOUNT-BYPass'; else echo 'mount-blocked'; fi
if umount secrets 2>/dev/null; then echo 'UMOUNT-BYPASS'; else echo 'umount-blocked'; fi
if cat secrets/token.txt >/dev/null 2>&1; then echo 'LEAK-AFTER-UMOUNT'; else echo 'still-blocked-after-umount'; fi
"#
        );
        let output = run(&workspace, &script);
        let got = text(&output);
        assert!(output.status.success(), "{}", got);
        assert!(got.contains("normal=normal-ok"), "{}", got);
        assert!(got.contains("src=pub fn ok()"), "{}", got);
        assert!(got.contains("system=ok"), "{}", got);
        assert!(got.contains("created=write-ok"), "{}", got);
        assert!(got.contains("mount-blocked"), "{}", got);
        assert!(got.contains("umount-blocked"), "{}", got);
        assert!(got.contains("still-blocked-after-umount"), "{}", got);
        assert!(!got.contains("MOUNT-BYPass"), "{}", got);
        assert!(!got.contains("UMOUNT-BYPASS"), "{}", got);
        assert!(!got.contains("LEAK-AFTER-UMOUNT"), "{}", got);
        for path in [
            "secrets/token.txt",
            ".env",
            ".ui/tool-output/cached.txt",
            "nested/secrets/token.txt",
            "vendor/keys/token.txt",
            "alias",
        ] {
            assert!(got.contains(&format!("blocked:{}", path)), "{}", got);
        }
        assert!(
            got.contains(&format!("blocked:{}/outside.txt", outside)),
            "{}",
            got
        );
        assert!(
            got.contains("blocked:/proc/self/root/etc/passwd"),
            "{}",
            got
        );
        for secret in [
            "secret-value",
            "env-value",
            "cached-value",
            "nested-secret",
            "declared-secret",
            "outside-value",
        ] {
            assert!(!got.contains(secret), "{}", got);
        }
        assert_eq!(
            std::fs::read_to_string(workspace.join("created.txt")).unwrap(),
            "write-ok\n"
        );

        std::fs::remove_dir_all(base).unwrap();
    }
}
