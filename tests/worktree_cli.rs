#![cfg(target_os = "linux")]
//! Real linked worktrees and bundles; all mutation stays inside disposable fixtures.
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn artifact(path: &Path) -> Value {
    let output = Command::new("sha256sum").arg(path).output().unwrap();
    assert!(output.status.success());
    json!({"sha256": String::from_utf8(output.stdout).unwrap().split_whitespace().next().unwrap(),
        "bytes": fs::metadata(path).unwrap().len()})
}

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    worktree: PathBuf,
    closeout: PathBuf,
    head: String,
}

impl Fixture {
    fn new() -> Self {
        Self::with_worktree_name("linked worktree")
    }

    fn with_worktree_name(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("owner");
        let worktree = dir.path().join(name);
        let evidence = dir.path().join("evidence");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&evidence).unwrap();
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Doty test fixture"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        fs::write(repo.join("source.txt"), "preserved source\n").unwrap();
        fs::write(repo.join(".gitignore"), "cache/\n").unwrap();
        git(&repo, &["add", "."]);
        let tree = git(&repo, &["write-tree"]);
        let commit = git(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                &tree,
                "-m",
                "fixture",
            ],
        );
        git(&repo, &["update-ref", "refs/heads/main", &commit]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "finalized",
                worktree.to_str().unwrap(),
            ],
        );
        let head = git(&repo, &["rev-parse", "finalized"]);
        let bundle = evidence.join("source.bundle");
        git(
            &repo,
            &["bundle", "create", bundle.to_str().unwrap(), "--all"],
        );
        let handoff = evidence.join("HANDOFF.md");
        fs::write(&handoff, "CI and PR completion remain pending.\n").unwrap();
        let closeout = evidence.join("removal-readiness.json");
        let document = json!({
            "readyForRemoval": true, "worktree": worktree, "owningRepository": repo,
            "candidate": head, "candidateTree": git(&repo, &["rev-parse", "HEAD^{tree}"]),
            "branch": "finalized", "handoff": handoff,
            "artifacts": {"source.bundle": artifact(&bundle), "HANDOFF.md": artifact(&handoff)},
            "providerSnapshot": {"hostedAccepted": false, "merged": false}
        });
        fs::write(&closeout, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        Self {
            _dir: dir,
            repo,
            worktree,
            closeout,
            head,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_doty"))
            .env_remove("CHAOSBOX_SCRATCH_WORK")
            .args(["--mode", "system", "worktree", "remove", "--closeout"])
            .arg(&self.closeout)
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }

    fn edit_manifest(&self, edit: impl FnOnce(&mut Value)) {
        let mut document = serde_json::from_slice(&fs::read(&self.closeout).unwrap()).unwrap();
        edit(&mut document);
        fs::write(&self.closeout, serde_json::to_vec(&document).unwrap()).unwrap();
    }

    fn rejects(&self, expected: &str) {
        let output = self.run(&["--apply"]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(self.worktree.exists());
        assert_eq!(git(&self.repo, &["rev-parse", "finalized"]), self.head);
    }
}

#[test]
fn user_mode_requires_owned_roots_and_can_remove_a_local_finalized_worktree() {
    let fixture = Fixture::new();
    let invoke = |home: &Path| {
        Command::new(env!("CARGO_BIN_EXE_doty"))
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("CHAOSBOX_SCRATCH_WORK")
            .env_remove("CHAOSBOX_SCRATCH_ROOT")
            .args(["worktree", "remove", "--closeout"])
            .arg(&fixture.closeout)
            .args(["--apply", "--json"])
            .output()
            .unwrap()
    };
    let unrelated = tempfile::tempdir().unwrap();
    let rejected = invoke(unrelated.path());
    assert!(!rejected.status.success());
    assert!(fixture.worktree.exists());
    let result = invoke(fixture._dir.path());
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!fixture.worktree.exists());
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "finalized"]),
        fixture.head
    );
    assert!(fixture.closeout.exists());
}

#[test]
fn preview_then_remove_preserves_branch_evidence_and_owner_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.repo.join("source.txt"), "unrelated owner edit\n").unwrap();
    let before = git(&fixture.repo, &["status", "--porcelain"]);
    let output = fixture.run(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(preview["removed"], false);
    assert_eq!(preview["verified_artifacts"], 2);
    assert!(fixture.worktree.exists());
    let output = fixture.run(&["--apply"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["removed"], true);
    assert!(!fixture.worktree.exists());
    assert!(!git(&fixture.repo, &["worktree", "list", "--porcelain"]).contains("linked worktree"));
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "finalized"]),
        fixture.head
    );
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), before);
    assert!(fixture.closeout.exists());
    assert!(
        fixture
            .closeout
            .parent()
            .unwrap()
            .join("source.bundle")
            .exists()
    );
}

#[test]
fn dirty_or_stale_worktrees_are_rejected() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join("source.txt"), "new work\n").unwrap();
    fixture.rejects("dirty");
    let fixture = Fixture::new();
    fixture
        .edit_manifest(|doc| doc["candidate"] = json!("0000000000000000000000000000000000000000"));
    fixture.rejects("HEAD");
}

#[test]
fn ancestor_closeout_rejects_artifacts_inside_the_worktree() {
    for internal_handoff in [false, true] {
        let mut fixture = Fixture::new();
        let evidence = fixture.closeout.parent().unwrap().to_owned();
        let cache = fixture.worktree.join("cache");
        fs::create_dir(&cache).unwrap();
        let internal = cache.join("HANDOFF.evidence");
        fs::write(&internal, "preserve this evidence\n").unwrap();
        let parent = fixture._dir.path();
        fs::copy(evidence.join("source.bundle"), parent.join("source.bundle")).unwrap();
        fs::copy(evidence.join("HANDOFF.md"), parent.join("HANDOFF.md")).unwrap();
        let mut doc: Value = serde_json::from_slice(&fs::read(&fixture.closeout).unwrap()).unwrap();
        let name = internal.strip_prefix(parent).unwrap().to_str().unwrap();
        doc["artifacts"][name] = artifact(&internal);
        doc["handoff"] = json!(if internal_handoff {
            internal.clone()
        } else {
            parent.join("HANDOFF.md")
        });
        fixture.closeout = parent.join("removal-readiness.json");
        fs::write(&fixture.closeout, serde_json::to_vec(&doc).unwrap()).unwrap();
        let before = fs::read(&fixture.closeout).unwrap();
        for args in [vec![], vec!["--apply"]] {
            let output = fixture.run(&args);
            assert!(!output.status.success(), "{internal_handoff}: {args:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("artifact must be outside"));
            assert!(fixture.worktree.exists());
            assert_eq!(fs::read(&internal).unwrap(), b"preserve this evidence\n");
            assert_eq!(fs::read(&fixture.closeout).unwrap(), before);
            assert_eq!(
                git(&fixture.repo, &["rev-parse", "finalized"]),
                fixture.head
            );
        }
    }
}

#[test]
fn active_no_checkout_bisect_blocks_worktree_removal() {
    let fixture = Fixture::new();
    git(&fixture.worktree, &["bisect", "start", "--no-checkout"]);
    let admin = PathBuf::from(git(&fixture.worktree, &["rev-parse", "--absolute-git-dir"]));
    let log = fs::read(admin.join("BISECT_LOG")).unwrap();
    assert!(admin.join("BISECT_START").exists());
    for args in [vec![], vec!["--apply"]] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("active Git operation: BISECT"));
        assert!(fixture.worktree.exists());
        assert_eq!(fs::read(admin.join("BISECT_LOG")).unwrap(), log);
        assert_eq!(
            git(&fixture.repo, &["rev-parse", "finalized"]),
            fixture.head
        );
    }
}

#[test]
fn ignored_nested_repositories_are_preserved() {
    let fixture = Fixture::new();
    let nested = fixture.worktree.join("cache/nested");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--initial-branch=main"]);
    fs::write(nested.join("unique.txt"), "not in the parent bundle").unwrap();
    git(&nested, &["add", "."]);
    let tree = git(&nested, &["write-tree"]);
    let head = git(
        &nested,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            &tree,
            "-m",
            "independent history",
        ],
    );
    git(&nested, &["update-ref", "refs/heads/main", &head]);
    assert!(git(&fixture.worktree, &["status", "--porcelain"]).is_empty());
    for args in [vec![], vec!["--apply"]] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("nested Git repository"));
        assert_eq!(git(&nested, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read_to_string(nested.join("unique.txt")).unwrap(),
            "not in the parent bundle"
        );
        assert!(fixture.worktree.exists());
    }
}

#[test]
fn ignored_nested_bare_repositories_are_preserved() {
    let fixture = Fixture::new();
    let nested = fixture.worktree.join("cache/bare.git");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--bare", "--initial-branch=main"]);
    git(&nested, &["read-tree", "--empty"]);
    let tree = git(&nested, &["write-tree"]);
    let head = git(
        &nested,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            &tree,
            "-m",
            "independent bare history",
        ],
    );
    git(&nested, &["update-ref", "refs/heads/main", &head]);
    assert!(!nested.join(".git").exists());
    assert!(git(&fixture.worktree, &["status", "--porcelain"]).is_empty());
    for args in [vec![], vec!["--apply"]] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("nested Git repository"));
        assert_eq!(git(&nested, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&nested, &["cat-file", "-t", &head]), "commit");
        assert!(fixture.worktree.exists());
    }
}

#[test]
fn ignored_nested_gitfiles_and_dangling_git_markers_are_preserved() {
    for symlink in [false, true] {
        let fixture = Fixture::new();
        let nested = fixture.worktree.join("cache/nested");
        fs::create_dir_all(&nested).unwrap();
        if symlink {
            std::os::unix::fs::symlink("/nonexistent-git-directory", nested.join(".git")).unwrap();
        } else {
            fs::write(nested.join(".git"), "gitdir: /external/git-directory\n").unwrap();
        }
        for args in [vec![], vec!["--apply"]] {
            let output = fixture.run(&args);
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("nested Git repository"));
            assert!(fs::symlink_metadata(nested.join(".git")).is_ok());
        }
    }
}

#[test]
fn changed_preservation_artifact_is_rejected() {
    let fixture = Fixture::new();
    fs::write(
        fixture.closeout.parent().unwrap().join("HANDOFF.md"),
        "changed\n",
    )
    .unwrap();
    fixture.rejects("artifact");
}

fn fake_chaosbox(fixture: &Fixture, link_result: &str, entries: Vec<Value>) -> Vec<String> {
    let parent = fixture.closeout.parent().unwrap();
    let root = parent.join("scratch");
    let allocation = root.join("allocation");
    fs::create_dir_all(&allocation).unwrap();
    fs::create_dir(parent.join("ledger")).unwrap();
    let report = json!({"version":1,"scope":"private:test","host":"testhost","root":root,
        "complete":true,"items":[{"path":allocation,"at":null,"blocked":true,"reasons":["open"],"entries":entries}]});
    fs::write(
        parent.join("query.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("sh"))
        .find(|path| path.is_file())
        .unwrap();
    let binary = parent.join("chaosbox");
    support::write_executable(
        &binary,
        &format!(
            "#!{}\nset -eu\ncd '{}'\nwhile [ $# -gt 0 ]; do case \"$1\" in query|link) action=$1; shift; break;; *) shift;; esac; done\nprintf '%s\\n' \"$action\" >> calls\nif [ \"$action\" = query ]; then cat query.json; else\n  test -d '{}'\n  printf '%s\\n' \"$@\" > link-args\n  {link_result}\nfi\n",
            shell.display(),
            parent.display(),
            fixture.worktree.display()
        ),
    );
    vec![
        "--chaosbox-work".into(),
        parent.join("ledger").display().to_string(),
        "--chaosbox-scope".into(),
        "private:test".into(),
        "--chaosbox-host".into(),
        "testhost".into(),
        "--chaosbox-root".into(),
        root.display().to_string(),
        "--chaosbox-bin".into(),
        binary.display().to_string(),
        "--chaosbox-allocation".into(),
        allocation.display().to_string(),
    ]
}

fn allocation_entry(fixture: &Fixture) -> Value {
    json!({"path":fixture.closeout.parent().unwrap().join("scratch/allocation"),
        "identity_matches":true,"identity":{"dev":"1","ino":"2","birth_ns":"3"},"links":[]})
}

#[test]
fn preservation_allocation_inside_worktree_is_rejected_before_linking() {
    let fixture = Fixture::new();
    let mut args = fake_chaosbox(
        &fixture,
        "printf '{\"version\":1,\"generation\":2}\\n'",
        vec![],
    );
    let root = fixture.worktree.join("cache");
    let allocation = root.join("allocation");
    fs::create_dir_all(&allocation).unwrap();
    let parent = fixture.closeout.parent().unwrap();
    let entry = json!({"path":allocation,"identity_matches":true,
        "identity":{"dev":"1","ino":"2","birth_ns":"3"},"links":[]});
    let report = json!({"version":1,"scope":"private:test","host":"testhost","root":root,
        "complete":true,"items":[{"path":allocation,"at":null,"blocked":true,"reasons":["open"],"entries":[entry]}]});
    fs::write(
        parent.join("query.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    for flag in ["--chaosbox-root", "--chaosbox-allocation"] {
        let index = args.iter().position(|a| a == flag).unwrap();
        args[index + 1] = if flag == "--chaosbox-root" {
            &root
        } else {
            &allocation
        }
        .display()
        .to_string();
    }
    for apply in [false, true] {
        let mut invocation: Vec<_> = args.iter().map(String::as_str).collect();
        if apply {
            invocation.push("--apply");
        }
        let output = fixture.run(&invocation);
        assert!(!output.status.success(), "apply={apply}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("allocation must be outside"));
        assert!(allocation.exists());
        assert!(fixture.worktree.exists());
        assert!(!parent.join("link-args").exists());
        assert!(!parent.join("calls").exists());
        assert_eq!(
            git(&fixture.repo, &["rev-parse", "finalized"]),
            fixture.head
        );
    }
}

#[test]
fn missing_allocation_and_link_failure_stop_before_removal() {
    let fixture = Fixture::new();
    let mut args = fake_chaosbox(&fixture, "exit 1", vec![]);
    args.push("--apply".into());
    let output = fixture.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("allocation missing"));
    assert!(fixture.worktree.exists());
    assert_eq!(
        fs::read_to_string(fixture.closeout.parent().unwrap().join("calls")).unwrap(),
        "query\n"
    );

    let fixture = Fixture::new();
    let mut args = fake_chaosbox(
        &fixture,
        "printf 'link unavailable\\n' >&2; exit 1",
        vec![allocation_entry(&fixture)],
    );
    args.push("--apply".into());
    let output = fixture.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("link unavailable"));
    assert!(fixture.worktree.exists());
}

#[test]
fn chaosbox_preview_is_read_only_and_apply_requires_link_readback() {
    let fixture = Fixture::new();
    let mut args = fake_chaosbox(
        &fixture,
        "printf '{\"version\":1,\"generation\":2}\\n'",
        vec![allocation_entry(&fixture)],
    );
    let borrowed: Vec<_> = args.iter().map(String::as_str).collect();
    let output = fixture.run(&borrowed);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.worktree.exists());
    assert!(
        !fixture
            .closeout
            .parent()
            .unwrap()
            .join("link-args")
            .exists()
    );
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(preview["chaosbox_link"]["recorded"], false);
    let mut report: Value = serde_json::from_slice(
        &fs::read(fixture.closeout.parent().unwrap().join("query.json")).unwrap(),
    )
    .unwrap();
    report["items"][0]["entries"][0]["links"] = json!([{"category":"preservation",
        "reference":preview["chaosbox_link"]["reference"],"description":preview["chaosbox_link"]["description"]}]);
    fs::write(
        fixture.closeout.parent().unwrap().join("linked.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    // Have the fake facade publish the readback only when link is called.
    let binary = fixture.closeout.parent().unwrap().join("chaosbox");
    let script = fs::read_to_string(&binary)
        .unwrap()
        .replace("printf '{", "cp linked.json query.json\n  printf '{");
    support::write_executable(&binary, &script);
    args.push("--apply".into());
    let output = fixture.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.worktree.exists());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["chaosbox_link"]["recorded"], true);
    let linked_args =
        fs::read_to_string(fixture.closeout.parent().unwrap().join("link-args")).unwrap();
    assert!(linked_args.contains(fixture.closeout.to_str().unwrap()));
    assert!(linked_args.contains(result["closeout_sha256"].as_str().unwrap()));
}

#[test]
fn missing_link_readback_stops_before_removal() {
    let fixture = Fixture::new();
    let mut args = fake_chaosbox(
        &fixture,
        "printf '{\"version\":1,\"generation\":2}\\n'",
        vec![allocation_entry(&fixture)],
    );
    args.push("--apply".into());
    let output = fixture.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("readback mismatch"));
    assert!(fixture.worktree.exists());
}

#[test]
fn untracked_changes_git_locks_and_ignored_protection_markers_block_removal() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join("new.txt"), "new work\n").unwrap();
    fixture.rejects("dirty");

    let fixture = Fixture::new();
    git(
        &fixture.repo,
        &["worktree", "lock", fixture.worktree.to_str().unwrap()],
    );
    fixture.rejects("locked");

    let fixture = Fixture::new();
    let admin = git(&fixture.worktree, &["rev-parse", "--absolute-git-dir"]);
    fs::write(Path::new(&admin).join("index.lock"), "").unwrap();
    fixture.rejects("active Git operation");

    let fixture = Fixture::new();
    let cache = fixture.worktree.join("cache");
    fs::create_dir(&cache).unwrap();
    std::os::unix::fs::symlink("missing", cache.join(".doty-protect")).unwrap();
    fixture.rejects("protected worktree subtree");
}

struct Worker(std::process::Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn active_worker_cwd_and_external_file_descriptor_block_removal() {
    for name in [
        "linked worktree",
        "linked worktree (deleted)",
        "linked worktree (deleted) (deleted)",
    ] {
        let fixture = Fixture::with_worktree_name(name);
        let _worker = Worker(
            Command::new("sleep")
                .arg("60")
                .current_dir(&fixture.worktree)
                .spawn()
                .unwrap(),
        );
        fixture.rejects("active worker");
    }

    let fixture = Fixture::new();
    // A worker can use checkout files even with its cwd and arguments elsewhere.
    let _worker = Worker(
        Command::new("sleep")
            .arg("60")
            .stdin(fs::File::open(fixture.worktree.join("source.txt")).unwrap())
            .spawn()
            .unwrap(),
    );
    fixture.rejects("active worker");
}

#[test]
fn active_worker_path_lists_and_structured_arguments_block_removal() {
    for suffix in [
        ":/other",
        ",mode=read",
        ";/other",
        ")",
        "]",
        "}",
        "=read",
        "?watch=1",
        "#revision",
        "@remote",
        "&watch=1",
        "!hold",
    ] {
        let fixture = Fixture::new();
        // The worker has no checkout fd or cwd. Its argv alone reserves it.
        let mut worker = Worker(
            Command::new("sh")
                .args(["-c", "read -r line"])
                .arg(format!(
                    "--search-path={}{}",
                    fixture.worktree.display(),
                    suffix
                ))
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        assert!(worker.0.try_wait().unwrap().is_none());
        fixture.rejects("command references the worktree");
    }
}

#[test]
fn percent_encoded_uri_arguments_block_removal() {
    let fixture = Fixture::new();
    let encoded = fixture
        .worktree
        .to_str()
        .unwrap()
        .replace('%', "%25")
        .replace(' ', "%20");
    let mut worker = Worker(
        Command::new("sh")
            .args(["-c", "read -r line"])
            .arg(format!("--source=file://{encoded}?watch=1#revision"))
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert!(worker.0.try_wait().unwrap().is_none());
    fixture.rejects("command references the worktree");
}

#[test]
fn active_worker_mapping_with_escaped_pathname_blocks_removal() {
    if let Some(path) = std::env::var_os("DOTY_FIXTURE_MAP_FILE") {
        use std::{io::Read, os::fd::AsRawFd};
        let file = fs::File::open(path).unwrap();
        let length = file.metadata().unwrap().len() as usize;
        // SAFETY: map the live fixture descriptor read-only, without dereferencing it.
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(mapping, libc::MAP_FAILED);
        drop(file);
        fs::write(std::env::var_os("DOTY_FIXTURE_MAP_READY").unwrap(), "ready").unwrap();
        let _ = std::io::stdin().read_exact(&mut [0]);
        // SAFETY: this is the exact mapping and length returned by mmap above.
        unsafe { libc::munmap(mapping, length) };
        return;
    }
    for name in ["linked\nworktree", "linked\\012\nworktree"] {
        let fixture = Fixture::with_worktree_name(name);
        let ready = fixture
            .closeout
            .parent()
            .unwrap()
            .join("mapped-worker-ready");
        let mut worker = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "active_worker_mapping_with_escaped_pathname_blocks_removal",
                ])
                .env("DOTY_FIXTURE_MAP_FILE", fixture.worktree.join("source.txt"))
                .env("DOTY_FIXTURE_MAP_READY", &ready)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let started = std::time::Instant::now();
        while !ready.exists() {
            assert!(worker.0.try_wait().unwrap().is_none());
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        fixture.rejects("maps worktree content");
    }
}

#[test]
fn unsafe_evidence_paths_wrong_owner_and_missing_bundle_candidate_are_rejected() {
    let fixture = Fixture::new();
    fixture.edit_manifest(|doc| {
        doc["artifacts"]["../escape"] = json!({"sha256":"0".repeat(64),"bytes":0})
    });
    fixture.rejects("artifact names");

    let fixture = Fixture::new();
    fixture
        .edit_manifest(|doc| doc["owningRepository"] = json!(fixture.closeout.parent().unwrap()));
    fixture.rejects("owning repository mismatch");

    let fixture = Fixture::new();
    let bundle = fixture.closeout.parent().unwrap().join("source.bundle");
    git(
        &fixture.repo,
        &["bundle", "create", bundle.to_str().unwrap(), "main"],
    );
    fixture.edit_manifest(|doc| doc["artifacts"]["source.bundle"] = artifact(&bundle));
    fixture.rejects("does not preserve the candidate branch");
}

#[test]
fn git_environment_cannot_redirect_removal() {
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_doty"))
        .env_remove("CHAOSBOX_SCRATCH_WORK")
        .env("GIT_DIR", fixture.repo.join(".git"))
        .env("GIT_WORK_TREE", &fixture.repo)
        .args(["--mode", "system", "worktree", "remove", "--closeout"])
        .arg(&fixture.closeout)
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.worktree.exists());
    assert!(fixture.repo.join("source.txt").exists());
}

#[test]
fn changes_during_linkage_are_rechecked_before_removal() {
    let fixture = Fixture::new();
    let mut args = fake_chaosbox(
        &fixture,
        "printf '{\"version\":1,\"generation\":2}\\n'",
        vec![allocation_entry(&fixture)],
    );
    let output = fixture.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(output.status.success());
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    let query = fixture.closeout.parent().unwrap().join("query.json");
    let mut report: Value = serde_json::from_slice(&fs::read(&query).unwrap()).unwrap();
    report["items"][0]["entries"][0]["links"] = json!([{"category":"preservation",
        "reference":preview["chaosbox_link"]["reference"],"description":preview["chaosbox_link"]["description"]}]);
    fs::write(
        fixture.closeout.parent().unwrap().join("linked.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    let binary = fixture.closeout.parent().unwrap().join("chaosbox");
    let script = fs::read_to_string(&binary).unwrap().replace(
        "printf '{",
        &format!(
            "cp linked.json query.json\nprintf changed > '{}'\n  printf '{{",
            fixture.worktree.join("source.txt").display()
        ),
    );
    support::write_executable(&binary, &script);
    args.push("--apply".into());
    let output = fixture.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("dirty"));
    assert!(fixture.worktree.exists());
}
