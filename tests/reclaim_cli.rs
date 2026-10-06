#![cfg(target_os = "linux")]
//! Exercise the real CLI against local fake cleanup tools, never the host's GC.
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(gc_script: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect();
        let resolve = |name: &str| {
            paths
                .iter()
                .map(|path| path.join(name))
                .find(|path| path.is_file())
                .unwrap()
        };
        let shell = resolve("sh");
        for name in ["sh", "sleep", "df", "findmnt"] {
            std::os::unix::fs::symlink(resolve(name), dir.path().join(name)).unwrap();
        }
        for (name, script) in [
            ("nh", "printf 'retention\\n' >> \"$RECLAIM_LOG\"\n"),
            ("nix-store", gc_script),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, format!("#!{}\n{script}", shell.display())).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(dir.path().join("targets.json"), r#"{"targets":[{"name":"nix-store-gc","variant":"nh-clean","settings":{"scope":"user"}}]}"#).unwrap();
        Self { dir }
    }

    fn command(&self, options: &[&str]) -> Command {
        let prefix = self.dir.path().to_str().unwrap();
        let mut command = Command::new(
            std::env::var("DOTY_TEST_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_doty").into()),
        );
        command
            .args(["reclaim", "--mount", "/nix", "--json", "--config"])
            .arg(self.dir.path().join("targets.json"))
            .args(options)
            // Never let a fixture interpreter failure fall through to real cleanup tools.
            .env("PATH", prefix)
            .env("RECLAIM_LOG", self.dir.path().join("commands.log"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    }

    fn run(&self, options: &[&str]) -> Output {
        self.command(options).output().unwrap()
    }

    fn report(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid report: {error}; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

#[test]
fn applied_cli_honours_gc_budget_and_reports_unmet_goal_as_failure() {
    let fixture = Fixture::new(
        "printf 'gc %s\\n' \"$3\" >> \"$RECLAIM_LOG\"\nprintf '1 store paths deleted, %s bytes freed (0 MiB)\\n' \"$3\" >&2\n",
    );
    let output = fixture.run(&[
        "--apply",
        "--min-free-bytes",
        "18446744073709551615",
        "--gc-max-bytes",
        "150",
        "--gc-pass-bytes",
        "100",
    ]);
    assert!(!output.status.success());
    let report = Fixture::report(&output);
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["filesystems"].as_array().unwrap().len(), 1);
    let fs = &report["filesystems"][0];
    assert_eq!(fs["goal_met"], false);
    assert!(fs["final_available_bytes"].as_u64().is_some());
    let target = &fs["targets"][0];
    assert_eq!(target["status"], "completed");
    assert!(target["estimated_freed_bytes"].is_null());
    assert_eq!(target["actual_freed_bytes"], 150);
    assert!(
        target["command_log"]
            .as_str()
            .unwrap()
            .contains("--max-freed 50")
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("commands.log")).unwrap(),
        "retention\ngc 100\ngc 50\n"
    );
}

#[test]
fn timeout_cli_preserves_json_and_partial_measurement() {
    let fixture = Fixture::new("printf 'partial GC progress\\n' >&2\nsleep 30 & wait\n");
    let started = std::time::Instant::now();
    let output = fixture.run(&[
        "--apply",
        "--min-free-bytes",
        "18446744073709551615",
        "--timeout-seconds",
        "1",
    ]);
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert!(!output.status.success());
    let report = Fixture::report(&output);
    let fs = &report["filesystems"][0];
    assert!(fs["final_available_bytes"].as_u64().is_some());
    assert_eq!(fs["targets"][0]["status"], "completed-with-errors");
    let notes = fs["targets"][0]["notes"].as_str().unwrap();
    assert!(notes.contains("runtime budget exhausted"));
    assert!(notes.contains("partial GC progress"));
}

#[test]
fn unknown_yield_is_successful_action_but_unmet_goal_still_fails_cli() {
    let fixture = Fixture::new(
        "printf 'gc\\n' >> \"$RECLAIM_LOG\"\nprintf 'done without a summary\\n' >&2\n",
    );
    let output = fixture.run(&["--apply", "--min-free-bytes", "18446744073709551615"]);
    assert!(!output.status.success());
    let report = Fixture::report(&output);
    let target = &report["filesystems"][0]["targets"][0];
    assert_eq!(target["status"], "completed");
    assert!(target["actual_freed_bytes"].is_null());
    assert!(target["notes"].as_str().unwrap().contains("yield unknown"));
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("commands.log")).unwrap(),
        "retention\ngc\n"
    );
}

#[test]
fn healthy_cli_keeps_json_format_and_executes_nothing() {
    let fixture = Fixture::new("exit 99\n");
    let output = fixture.run(&["--apply", "--min-free-bytes", "0"]);
    assert!(output.status.success());
    let report = Fixture::report(&output);
    assert!(report["filesystems"].as_array().unwrap().is_empty());
    assert!(!fixture.dir.path().join("commands.log").exists());
    let output = fixture.run(&["--apply", "--all", "--min-free-bytes", "0"]);
    assert!(output.status.success());
    let report = Fixture::report(&output);
    assert_eq!(report["filesystems"][0]["goal_met"], true);
    assert_eq!(report["filesystems"][0]["targets"][0]["status"], "skipped");
    assert!(!fixture.dir.path().join("commands.log").exists());
}

#[test]
fn sigterm_cli_cancels_cleanup_and_keeps_partial_receipt() {
    let fixture = Fixture::new("printf 'GC started\\n' >> \"$RECLAIM_LOG\"\nsleep 30 & wait\n");
    let mut child = fixture
        .command(&[
            "--apply",
            "--min-free-bytes",
            "18446744073709551615",
            "--timeout-seconds",
            "5",
        ])
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while !fs::read_to_string(fixture.dir.path().join("commands.log"))
        .unwrap_or_default()
        .contains("GC started")
    {
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // SAFETY: this is the live fixture subprocess we spawned, not a host cleanup.
    assert_eq!(
        unsafe { libc::kill(child.id().try_into().unwrap(), libc::SIGTERM) },
        0
    );
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let report = Fixture::report(&output);
    assert!(
        report["filesystems"][0]["final_available_bytes"]
            .as_u64()
            .is_some()
    );
    assert!(
        report["filesystems"][0]["targets"][0]["notes"]
            .as_str()
            .unwrap()
            .contains("reclaim interrupted")
    );
    assert!(
        report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("interrupted"))
    );
}
