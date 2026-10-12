//! Real filesystem scope checks; subprocess tools are disposable fakes.
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::{Command, Output},
};

struct Fixture {
    home: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/doty")).unwrap();
        fs::create_dir(home.path().join("bin")).unwrap();
        Self { home }
    }
    fn config(&self, mode: &str, targets: Value) {
        fs::write(
            self.home.path().join(".config/doty/targets.json"),
            json!({"mode": mode, "targets": targets}).to_string(),
        )
        .unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_doty"))
            .env("HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("CHAOSBOX_SCRATCH_ROOT")
            .env_remove("CHAOSBOX_SCRATCH_WORK")
            .env_remove("CHAOSBOX_SCRATCH_BIN")
            .env("PATH", self.home.path().join("bin"))
            .env_remove("DOTY_STATE_DIR")
            .args(args)
            .output()
            .unwrap()
    }
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn default_user_configuration_and_cache_paths_are_local() {
    let f = Fixture::new();
    let cache = f.home.path().join(".cache/go-build");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("recent"), "keep").unwrap();
    f.config(
        "user",
        json!([{"name":"package-caches", "variant":"disk-report",
        "settings":{"roots":[f.home.path().join(".cache")], "paths":[cache]}}]),
    );
    let result = success(f.run(&["status", "--json"]));
    assert_eq!(result["inspections"][0]["path"], cache.to_str().unwrap());
    assert!(cache.join("recent").exists());
}

#[test]
fn user_doctor_checks_resolved_local_provider_paths() {
    let f = Fixture::new();
    let config = f.home.path().join(".config/doty/targets.json");
    fs::write(&config, json!({
        "mode":"user", "targets":[{"name":"uv-cache","variant":"clean"}],
        "requirements":[{"name":"uv-cache","variant":"clean","path":f.home.path().join(".cache/uv")}]
    }).to_string()).unwrap();
    let result = success(f.run(&["doctor", "--json"]));
    assert_eq!(result["coverage_gaps"], json!([]));
    assert!(
        result["extra"]
            .as_array()
            .unwrap()
            .iter()
            .all(|pair| !pair.as_str().unwrap().starts_with("nix-store-gc/"))
    );
}

#[test]
fn system_document_cannot_be_selected_in_user_mode() {
    let f = Fixture::new();
    f.config("system", json!([]));
    let output = f.run(&["status", "--json"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--mode system"));
}

#[test]
fn force_and_explicit_configuration_cannot_enable_system_targets() {
    let f = Fixture::new();
    f.config(
        "user",
        json!([{"name":"nix-store-gc", "variant":"nix-collect-garbage"}]),
    );
    for command in ["run", "reclaim"] {
        let output = f.run(&[
            command,
            "--config",
            f.home
                .path()
                .join(".config/doty/targets.json")
                .to_str()
                .unwrap(),
            "--apply",
            "--force",
            "--json",
        ]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("system-only"));
    }
}

#[test]
fn user_cache_roots_cannot_escape_through_settings_or_symlinks() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let link = f.home.path().join("linked");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    for path in [outside.path(), &link] {
        f.config(
            "user",
            json!([{"name":"package-caches", "variant":"prune-stale",
            "settings":{"roots":[path], "paths":[path]}}]),
        );
        let output = f.run(&["run", "--apply", "--force", "--json"]);
        assert!(!output.status.success());
    }
}

#[test]
fn standalone_removal_rejects_outside_root_before_writing_a_plan() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), "keep").unwrap();
    let output = f.run(&[
        "rm",
        "--root",
        outside.path().to_str().unwrap(),
        "--apply",
        "--",
        outside.path().join("keep").to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(outside.path().join("keep").exists());
    assert!(!f.home.path().join(".local/state/doty").exists());
}

#[test]
fn report_only_force_does_not_delete() {
    let f = Fixture::new();
    let cache = f.home.path().join("cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("keep"), "data").unwrap();
    f.config("system", json!([{"name":"package-caches", "variant":"disk-report", "settings":{"roots":[cache],"paths":[cache]}}]));
    let result = success(
        f.run(&[
            "--mode",
            "system",
            "run",
            "--config",
            f.home
                .path()
                .join(".config/doty/targets.json")
                .to_str()
                .unwrap(),
            "--apply",
            "--force",
            "--json",
        ]),
    );
    assert_eq!(result["reports"][0]["removed"], 0);
    assert!(
        result["reports"][0]["errors"][0]
            .as_str()
            .unwrap()
            .contains("report-only")
    );
    assert!(cache.join("keep").exists());
}

#[test]
fn uv_inspection_and_execution_use_the_same_explicit_directory() {
    let f = Fixture::new();
    let cache = f.home.path().join(".cache/uv");
    fs::create_dir_all(&cache).unwrap();
    let bin = f.home.path().join("bin");
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("sh"))
        .find(|p| p.is_file())
        .unwrap();
    let tool = bin.join("uv");
    fs::write(
        &tool,
        format!(
            "#!{}\nprintf '%s\\n' \"$@\" > \"$HOME/uv-args\"\n",
            shell.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    f.config("user", json!([{"name":"uv-cache", "variant":"clean"}]));
    let result = success(f.run(&["status", "--json"]));
    assert_eq!(result["inspections"][0]["path"], cache.to_str().unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_doty"))
        .env("HOME", f.home.path())
        .env("XDG_CACHE_HOME", f.home.path().join(".cache"))
        .env("XDG_CONFIG_HOME", f.home.path().join(".config"))
        .env("PATH", &bin)
        .args(["run", "--apply", "--json"])
        .output()
        .unwrap();
    success(output);
    assert_eq!(
        fs::read_to_string(f.home.path().join("uv-args")).unwrap(),
        format!("cache\nclean\n--cache-dir\n{}\n", cache.display())
    );
}

#[test]
fn user_trash_is_local_and_confirmed() {
    let f = Fixture::new();
    let trash = f.home.path().join(".local/share/Trash/files");
    fs::create_dir_all(&trash).unwrap();
    fs::write(trash.join("discard"), "data").unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), "keep").unwrap();
    std::os::unix::fs::symlink(outside.path(), trash.join("link")).unwrap();
    f.config(
        "user",
        json!([{"name":"freedesktop-trash", "variant":"empty"}]),
    );
    success(f.run(&["run", "--apply", "--json"]));
    assert!(trash.join("discard").exists());
    success(f.run(&["run", "--apply", "--force", "--json"]));
    assert!(!trash.join("discard").exists());
    assert!(outside.path().join("keep").exists());
    assert!(fs::symlink_metadata(trash.join("link")).is_err());
}

#[test]
fn user_trash_rejects_symlinked_structural_directories() {
    for name in ["files", "info"] {
        let f = Fixture::new();
        let trash = f.home.path().join(".local/share/Trash");
        fs::create_dir_all(&trash).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("keep");
        fs::write(&sentinel, "outside caller roots").unwrap();
        std::os::unix::fs::symlink(outside.path(), trash.join(name)).unwrap();
        f.config(
            "user",
            json!([{"name":"freedesktop-trash", "variant":"empty"}]),
        );
        for args in [
            vec!["status", "--json"],
            vec!["run", "--apply", "--force", "--json"],
            vec!["reclaim", "--apply", "--force", "--json"],
        ] {
            let output = f.run(&args);
            assert!(!output.status.success(), "{name}: {args:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("symlink"));
            assert_eq!(
                fs::read_to_string(&sentinel).unwrap(),
                "outside caller roots"
            );
        }
    }
}

#[test]
fn foreign_and_shared_writable_roots_are_rejected() {
    let f = Fixture::new();
    let output = f.run(&["--user-root", "/etc", "list", "--json"]);
    assert!(!output.status.success());
    let cache = f.home.path().join("shared");
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o777)).unwrap();
    f.config(
        "user",
        json!([{"name":"uv-cache","variant":"clean","settings":{"cacheDir":cache}}]),
    );
    assert!(
        !f.run(&["run", "--apply", "--force", "--json"])
            .status
            .success()
    );
}

#[test]
fn restore_and_purge_reject_forged_external_plan_paths() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let plans = f.home.path().join(".local/state/doty/rm-plans");
    fs::create_dir_all(&plans).unwrap();
    fs::write(
        plans.join("forged.json"),
        json!({
            "id":"forged", "created_at":"test", "root":outside.path(),
            "quarantine_dir":outside.path().join(".doty-quarantine"), "targets":[]
        })
        .to_string(),
    )
    .unwrap();
    for args in [
        vec!["restore", "forged", "--json"],
        vec!["purge", "forged", "--apply", "--json"],
    ] {
        let output = f.run(&args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("outside caller roots"));
    }
}

#[test]
fn purge_rejects_forged_quarantine_relationships_without_mutation() {
    for mode in ["user", "system"] {
        for (quarantine_name, payload) in [
            ("ordinary-data", "unrelated-file"),
            (".doty-quarantine", "unrelated-file"),
            (".doty-quarantine", "nested/forged-0-original"),
        ] {
            let f = Fixture::new();
            let root = f.home.path().join("scratch");
            let quarantine = root.join(quarantine_name);
            let sentinel = quarantine.join(payload);
            fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
            fs::write(&sentinel, "not quarantined by doty").unwrap();
            let meta = fs::metadata(&sentinel).unwrap();
            let plans = f.home.path().join(".local/state/doty/rm-plans");
            fs::create_dir_all(&plans).unwrap();
            let plan_file = plans.join("forged.json");
            let contents = json!({
                "id":"forged", "created_at":"test", "root":root,
                "quarantine_dir":quarantine, "targets":[{
                    "path":root.join("original"), "dev":meta.dev(), "ino":meta.ino(),
                    "is_dir":false, "size":meta.len(), "mtime_secs":meta.mtime(),
                    "mtime_nanos":meta.mtime_nsec(), "quarantined_as":sentinel
                }]
            })
            .to_string();
            fs::write(&plan_file, &contents).unwrap();
            for args in [
                vec!["purge", "forged", "--apply", "--json"],
                vec!["purge", "forged", "--json"],
                vec!["restore", "forged", "--json"],
                vec!["rm", "--plan", "forged", "--apply", "--json"],
            ] {
                let mut invocation = vec!["--mode", mode];
                invocation.extend(args);
                let output = f.run(&invocation);
                assert!(
                    !output.status.success(),
                    "{mode}: {quarantine_name}: {payload}"
                );
                assert!(String::from_utf8_lossy(&output.stderr).contains("quarantine"));
                assert_eq!(
                    fs::read_to_string(&sentinel).unwrap(),
                    "not quarantined by doty"
                );
                assert_eq!(fs::read_to_string(&plan_file).unwrap(), contents);
            }
        }
    }
}

#[test]
fn user_retention_preserves_recent_descendants_and_unverifiable_candidates() {
    let f = Fixture::new();
    for tool in ["df", "findmnt"] {
        let executable = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join(tool))
            .find(|p| p.is_file())
            .unwrap();
        std::os::unix::fs::symlink(executable, f.home.path().join("bin").join(tool)).unwrap();
    }
    let cache = f.home.path().join(".cache/go-build");
    fs::create_dir_all(cache.join("old")).unwrap();
    fs::create_dir_all(cache.join("recent-descendant")).unwrap();
    fs::write(cache.join("old/file"), "old").unwrap();
    fs::write(cache.join("recent-descendant/keep"), "keep").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86400);
    for path in ["old/file", "old", "recent-descendant"] {
        fs::File::open(cache.join(path))
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    f.config(
        "user",
        json!([{"name":"package-caches","variant":"prune-stale","settings":{
            "roots":[f.home.path()],"paths":[cache],"minAgeDays":30
        }}]),
    );
    let inspection = success(f.run(&["status", "--json"]));
    let eligible = inspection["inspections"][0]["would_remove"]
        .as_u64()
        .unwrap();
    assert!(eligible <= 1, "{inspection}");
    assert!(
        inspection["inspections"][0]["notes"]
            .as_str()
            .unwrap()
            .contains("recent descendant activity")
    );
    if eligible == 0 {
        assert!(
            inspection["inspections"][0]["notes"]
                .as_str()
                .unwrap()
                .contains("process use cannot be verified"),
            "{inspection}"
        );
    }
    let result = success(f.run(&["run", "--apply", "--force", "--json"]));
    let removed = result["reports"][0]["removed"].as_u64().unwrap();
    assert!(removed <= eligible, "{result}");
    assert_eq!(cache.join("old").exists(), removed == 0);
    assert!(cache.join("recent-descendant/keep").exists());
}

#[test]
fn user_removal_restore_and_purge_round_trip_stays_in_owned_scratch() {
    let f = Fixture::new();
    let root = f.home.path().join(".cache/doty/scratch");
    fs::create_dir_all(root.join("discard")).unwrap();
    fs::write(root.join("discard/file"), "bytes").unwrap();
    let output = success(f.run(&[
        "rm",
        "--root",
        root.to_str().unwrap(),
        "--apply",
        "--json",
        "--",
        root.join("discard").to_str().unwrap(),
    ]));
    let plan = output["id"].as_str().unwrap();
    assert!(!root.join("discard").exists());
    success(f.run(&["restore", plan, "--json"]));
    assert_eq!(fs::read(root.join("discard/file")).unwrap(), b"bytes");
    let output = success(f.run(&[
        "rm",
        "--root",
        root.to_str().unwrap(),
        "--apply",
        "--json",
        "--",
        root.join("discard").to_str().unwrap(),
    ]));
    success(f.run(&["purge", output["id"].as_str().unwrap(), "--apply", "--json"]));
    assert!(!root.join("discard").exists());
}
