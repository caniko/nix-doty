use super::*;
use std::fs;
use std::os::unix::fs::symlink;
use std::time::{Duration, SystemTime};

fn model_receipt(root: &Path, name: &str) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("weights.safetensors"), b"weights").unwrap();
    fs::write(
        path.join("ready.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1, "kind": "colibri-dir", "repo": "owner/model", "rev": "abc",
            "completedAtUtc": "2026-09-16T00:00:00Z", "totalBytes": 7,
            "files": [{"name": "weights.safetensors", "sizeBytes": 7}]
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

fn managed_report(settings: Value) -> Value {
    let variant = crate::registry::find_variant("managed-models", "inventory")
        .expect("managed downloads need an inventory provider");
    let inspection = variant.inspect_with_settings(&settings).unwrap();
    serde_json::from_str(&inspection.notes).unwrap()
}

#[test]
fn downloaded_retired_model_is_discovered_without_a_manual_paths_list() {
    let root = tempfile::tempdir().unwrap();
    let path = model_receipt(root.path(), "retired");
    let report = managed_report(serde_json::json!({
        "roots": [root.path()],
        "models": [{"id": "glm", "path": path, "repo": "owner/model", "rev": "abc",
            "lifecycle": "retired", "retiredAt": "2026-09-18T00:00:00Z"}]
    }));
    assert_eq!(report["artifacts"][0]["lifecycle"], "retired");
    assert!(report["artifacts"][0]["logicalBytes"].as_u64().unwrap() >= 7);
    assert!(path.join("weights.safetensors").exists());
}

#[test]
fn removed_profile_stays_visible_as_an_orphan_and_is_never_selected() {
    let root = tempfile::tempdir().unwrap();
    let path = model_receipt(root.path(), "orphan");
    let report = managed_report(serde_json::json!({"roots": [root.path()]}));
    assert_eq!(report["artifacts"][0]["lifecycle"], "orphaned");
    assert_eq!(report["artifacts"][0]["eligible"], false);
    assert!(path.exists());
}

#[test]
fn model_manifest_traversal_is_reported_and_never_followed() {
    let root = tempfile::tempdir().unwrap();
    let path = model_receipt(root.path(), "bad");
    fs::write(path.join("ready.json"), br#"{"schemaVersion":1,"kind":"colibri-dir","repo":"owner/model","rev":"abc","totalBytes":7,"files":[{"name":"../escape","sizeBytes":7}]}"#).unwrap();
    let report = managed_report(serde_json::json!({"roots": [root.path()]}));
    assert_eq!(report["complete"], false);
    assert_eq!(report["artifacts"][0]["eligible"], false);
    assert!(
        report["artifacts"][0]["blockers"]
            .to_string()
            .contains("relative")
    );
}

#[test]
fn retained_and_shared_consumer_models_are_not_retired_by_disabled_profiles() {
    let root = tempfile::tempdir().unwrap();
    let path = model_receipt(root.path(), "comparison");
    let report = managed_report(serde_json::json!({
        "roots": [root.path()], "pinnedPaths": [path],
        "models": [{"id": "comparison", "path": path, "repo": "owner/model", "rev": "abc", "lifecycle": "retained"}]
    }));
    assert_eq!(report["artifacts"][0]["lifecycle"], "active");
    assert_eq!(report["artifacts"][0]["eligible"], false);
}

fn settings(root: &Path, paths: &[&str]) -> Settings {
    let mut settings = Settings::parse(&serde_json::json!({
        "roots": [root],
        "paths": paths.iter().map(|p| root.join(p)).collect::<Vec<_>>(),
        "minAgeDays": 30
    }))
    .unwrap();
    settings.check_processes = false;
    settings
}

fn backdate(path: &Path) {
    let file = fs::File::open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(60 * 86400))
        .unwrap();
}

#[test]
fn stale_parent_never_authorizes_recent_descendants() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("cache")).unwrap();
    fs::create_dir(root.path().join("cache/old")).unwrap();
    fs::write(root.path().join("cache/old/item"), b"old").unwrap();
    backdate(&root.path().join("cache/old/item"));
    backdate(&root.path().join("cache/old"));
    fs::create_dir(root.path().join("cache/recent")).unwrap();
    fs::write(root.path().join("cache/recent/item"), b"recent").unwrap();
    backdate(&root.path().join("cache/recent"));
    backdate(&root.path().join("cache"));
    let report = select(Kind::Packages, &settings(root.path(), &["cache"])).unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].path, root.path().join("cache/old"));
    assert_eq!(report.candidates[0].scan.logical_bytes, 3);
    assert!(root.path().join("cache/recent/item").exists());
}

#[test]
fn deep_cache_file_selection_preserves_bucket_directories() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cache/hash")).unwrap();
    fs::write(root.path().join("cache/hash/old"), b"old").unwrap();
    fs::write(root.path().join("cache/hash/recent"), b"recent").unwrap();
    backdate(&root.path().join("cache/hash/old"));
    let mut policy = settings(root.path(), &["cache"]);
    policy.entry_depth = 2;
    let selected = select(Kind::Packages, &policy).unwrap();
    assert_eq!(selected.candidates.len(), 1);
    assert_eq!(
        selected.candidates[0].path,
        root.path().join("cache/hash/old")
    );
    assert!(root.path().join("cache/hash/recent").exists());
}

#[test]
fn cargo_incremental_selection_keeps_dependencies_and_evidence() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cargo/debug/.fingerprint")).unwrap();
    fs::create_dir_all(root.path().join("cargo/debug/incremental/crate")).unwrap();
    fs::create_dir_all(root.path().join("cargo/debug/deps")).unwrap();
    fs::write(
        root.path()
            .join("cargo/debug/incremental/crate/query-cache.bin"),
        b"cache",
    )
    .unwrap();
    fs::write(root.path().join("cargo/debug/deps/keep.rlib"), b"keep").unwrap();
    for path in [
        "cargo/debug/incremental/crate/query-cache.bin",
        "cargo/debug/incremental/crate",
        "cargo/debug/incremental",
    ] {
        backdate(&root.path().join(path));
    }
    let report = select(Kind::Incremental, &settings(root.path(), &["cargo"])).unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(
        report.candidates[0].path,
        root.path().join("cargo/debug/incremental/crate")
    );
    assert!(root.path().join("cargo/debug/deps/keep.rlib").exists());
}

#[test]
fn incomplete_scans_and_nested_protection_are_not_candidates() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cache/protected")).unwrap();
    fs::write(root.path().join("cache/protected/.doty-protect"), b"hold").unwrap();
    let policy = settings(root.path(), &["cache"]);
    assert!(
        select(Kind::Packages, &policy)
            .unwrap()
            .candidates
            .is_empty()
    );
    fs::remove_file(root.path().join("cache/protected/.doty-protect")).unwrap();
    fs::write(root.path().join("cache/protected/old"), b"old").unwrap();
    backdate(&root.path().join("cache/protected/old"));
    backdate(&root.path().join("cache/protected"));
    let mut bounded = policy;
    bounded.max_entries = 1;
    let report = select(Kind::Packages, &bounded).unwrap();
    assert!(report.candidates.is_empty());
    assert!(!report.complete);
}

#[test]
fn a_dangling_ancestor_protection_marker_is_still_a_hold() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cache/old")).unwrap();
    backdate(&root.path().join("cache/old"));
    let policy = settings(root.path(), &["cache"]);
    symlink("missing", root.path().join(".doty-protect")).unwrap();
    assert!(select(Kind::Packages, &policy).is_err());
    assert!(root.path().join("cache/old").exists());
}

#[test]
fn excluded_tmp_and_symlink_roots_cannot_be_deleted() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cache/entry/tmp")).unwrap();
    fs::write(root.path().join("cache/entry/tmp/keep"), b"work").unwrap();
    symlink(outside.path(), root.path().join("link")).unwrap();
    assert!(
        select(Kind::Packages, &settings(root.path(), &["cache"]))
            .unwrap()
            .candidates
            .is_empty()
    );
    assert!(select(Kind::Packages, &settings(root.path(), &["link"])).is_err());
    let mut policy = settings(root.path(), &["cache"]);
    policy.roots = vec![root.path().join("link")];
    policy.paths = vec![root.path().join("link")];
    assert!(select(Kind::Packages, &policy).is_err());
}

#[test]
fn retired_models_require_explicit_selection_and_respect_pins() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("old-model")).unwrap();
    fs::write(root.path().join("old-model/weights"), b"weights").unwrap();
    backdate(&root.path().join("old-model/weights"));
    backdate(&root.path().join("old-model"));
    let mut policy = settings(root.path(), &["old-model"]);
    assert!(select(Kind::Models, &policy).unwrap().candidates.is_empty());
    policy.retired_paths = policy.paths.clone();
    assert_eq!(select(Kind::Models, &policy).unwrap().candidates.len(), 1);
    policy.pinned_paths = policy.paths.clone();
    assert!(select(Kind::Models, &policy).unwrap().candidates.is_empty());
}

#[test]
fn retirement_requires_allowlist_and_never_selects_repositories() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("campaign")).unwrap();
    fs::write(root.path().join("campaign/data"), b"data").unwrap();
    backdate(&root.path().join("campaign/data"));
    backdate(&root.path().join("campaign"));
    let mut policy = settings(root.path(), &["campaign"]);
    assert!(
        select(Kind::Retirement, &policy)
            .unwrap()
            .candidates
            .is_empty()
    );
    policy.approved_paths = policy.paths.clone();
    assert_eq!(
        select(Kind::Retirement, &policy).unwrap().candidates.len(),
        1
    );
    fs::create_dir(root.path().join("campaign/.git")).unwrap();
    assert!(
        select(Kind::Retirement, &policy)
            .unwrap()
            .candidates
            .is_empty()
    );
}

#[test]
fn dry_run_does_not_create_a_plan_or_mutate_candidates() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cache/old")).unwrap();
    fs::write(root.path().join("cache/old/item"), b"old").unwrap();
    backdate(&root.path().join("cache/old/item"));
    backdate(&root.path().join("cache/old"));
    let policy = serde_json::json!({"roots": [root.path()], "paths": [root.path().join("cache")]});
    let report = PACKAGE_PRUNE
        .apply_with_settings(false, false, &policy)
        .unwrap();
    assert_eq!(report.removed, 0);
    assert_eq!(report.freed_bytes, 0);
    assert!(!root.path().join(".doty-quarantine").exists());
    assert!(root.path().join("cache/old/item").exists());
}

#[test]
fn open_files_are_active_even_when_backdated() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cache/old")).unwrap();
    fs::write(root.path().join("cache/old/item"), b"old").unwrap();
    backdate(&root.path().join("cache/old/item"));
    backdate(&root.path().join("cache/old"));
    let _open = fs::File::open(root.path().join("cache/old/item")).unwrap();
    let uid = fs::metadata(root.path()).unwrap().uid();
    assert!(
        process_paths(uid)
            .unwrap()
            .references(&root.path().join("cache"))
    );
    let mut policy = settings(root.path(), &["cache"]);
    policy.check_processes = true;
    assert!(
        select(Kind::Packages, &policy)
            .unwrap()
            .candidates
            .is_empty()
    );
}

#[test]
fn changed_descendant_invalidates_prepared_candidate() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cache/old")).unwrap();
    fs::write(root.path().join("cache/old/item"), b"old").unwrap();
    backdate(&root.path().join("cache/old/item"));
    backdate(&root.path().join("cache/old"));
    let policy = settings(root.path(), &["cache"]);
    let selected = select(Kind::Packages, &policy).unwrap();
    fs::write(root.path().join("cache/old/item"), b"new work").unwrap();
    assert!(recheck(&selected.candidates[0], &policy).is_err());
    assert!(root.path().join("cache/old/item").exists());
}

#[test]
fn cross_target_incremental_and_release_only_outputs_are_supported() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cargo/release/.fingerprint")).unwrap();
    fs::create_dir_all(
        root.path()
            .join("cargo/wasm32-unknown-unknown/release/incremental/crate"),
    )
    .unwrap();
    for path in [
        "cargo/wasm32-unknown-unknown/release/incremental/crate",
        "cargo/wasm32-unknown-unknown/release/incremental",
    ] {
        backdate(&root.path().join(path));
    }
    let policy = settings(root.path(), &["cargo"]);
    let selected = select(Kind::Incremental, &policy).unwrap();
    assert_eq!(selected.candidates.len(), 1);
    assert_eq!(
        selected.candidates[0].path,
        root.path()
            .join("cargo/wasm32-unknown-unknown/release/incremental/crate")
    );
}

#[test]
fn retention_configuration_cannot_disable_process_checks_or_escape_roots() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        Settings::parse(
            &serde_json::json!({"roots": [root.path()], "paths": [root.path().join("cache")]})
        )
        .unwrap()
        .check_processes
    );
    for invalid in [
        serde_json::json!({"roots": [root.path()], "paths": [root.path()], "checkProcesses": false}),
        serde_json::json!({"roots": [root.path()], "paths": [root.path().join("tmp/build")]}),
        serde_json::json!({"roots": [root.path()], "paths": [root.path().join("cache"), root.path().join("cache/nested")]}),
        serde_json::json!({"roots": [root.path()], "paths": ["/outside"]}),
    ] {
        assert!(Settings::parse(&invalid).is_err());
    }
}

#[test]
fn logical_candidate_bytes_are_not_a_reclaim_estimate() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("cache/old")).unwrap();
    fs::write(root.path().join("cache/old/item"), b"old").unwrap();
    backdate(&root.path().join("cache/old/item"));
    backdate(&root.path().join("cache/old"));
    let value = serde_json::json!({"roots": [root.path()], "paths": [root.path().join("cache")]});
    let inspection = PACKAGE_PRUNE.inspect_with_settings(&value).unwrap();
    assert_eq!(inspection.size_bytes, None);
    assert!(inspection.notes.contains("logical bytes"));
    assert_eq!(
        PACKAGE_REPORT
            .inspect_with_settings(&value)
            .unwrap()
            .size_bytes,
        Some(3)
    );
}

#[test]
fn retention_apply_round_trip_in_isolated_process() {
    // rm tests also use DOTY_STATE_DIR. Isolate this test in a subprocess
    // rather than mutating the test runner's global environment.
    let Some(root) = std::env::var_os("DOTY_RETENTION_FIXTURE") else {
        let fixture = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "targets::retention::tests::retention_apply_round_trip_in_isolated_process",
                "--nocapture",
            ])
            .env("DOTY_RETENTION_FIXTURE", fixture.path())
            .env("DOTY_STATE_DIR", fixture.path().join("state"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let root = PathBuf::from(root);
    fs::create_dir_all(root.join("cache/old")).unwrap();
    fs::write(root.join("cache/old/item"), b"old").unwrap();
    backdate(&root.join("cache/old/item"));
    backdate(&root.join("cache/old"));
    let policy = settings(&root, &["cache"]);
    assert_eq!(
        PACKAGE_PRUNE
            .apply_settings(true, false, &policy)
            .unwrap()
            .removed,
        0
    );
    assert!(root.join("cache/old/item").exists());
    let applied = PACKAGE_PRUNE.apply_settings(true, true, &policy).unwrap();
    assert_eq!(applied.removed, 1);
    assert!(!root.join("cache/old").exists());
    assert!(root.join("cache").exists());

    fs::create_dir(root.join("legacy")).unwrap();
    fs::write(root.join("legacy/db"), b"preserved").unwrap();
    backdate(&root.join("legacy/db"));
    backdate(&root.join("legacy"));
    let mut policy = settings(&root, &["legacy"]);
    policy.approved_paths = policy.paths.clone();
    let applied = RETIREMENT_QUARANTINE
        .apply_settings(true, true, &policy)
        .unwrap();
    assert_eq!(applied.removed, 1);
    assert_eq!(applied.freed_bytes, 0);
    assert!(!root.join("legacy").exists());
    let plans: Vec<_> = fs::read_dir(root.join("state/rm-plans"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    let retirement = plans
        .iter()
        .map(|path| serde_json::from_slice::<crate::rm::RmPlan>(&fs::read(path).unwrap()).unwrap())
        .find(|plan| {
            plan.targets
                .iter()
                .any(|target| target.path == root.join("legacy"))
        })
        .unwrap();
    assert!(!retirement.targets[0].purged);
    assert!(
        retirement.targets[0]
            .quarantined_as
            .as_ref()
            .unwrap()
            .join("db")
            .exists()
    );
    crate::rm::restore(&retirement.id).unwrap();
    assert_eq!(fs::read(root.join("legacy/db")).unwrap(), b"preserved");
}
