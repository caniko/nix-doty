//! Closeout-based removal of a registered linked checkout through ordinary Git.
//! Evidence and branch refs stay external; preview does not write any state.
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    process::{Command as Process, Stdio},
};

#[derive(Subcommand)]
pub enum Command {
    /// Verify preserved evidence and remove a linked checkout (preview by default)
    Remove {
        /// Existing removal-readiness.json with external artifact hashes
        #[arg(long)]
        closeout: PathBuf,
        /// Actually run git worktree remove, without force
        #[arg(long)]
        apply: bool,
        /// Existing Chaosbox allocation to receive the preservation link
        #[arg(long)]
        chaosbox_allocation: Option<PathBuf>,
        /// Output the verification/removal result as JSON
        #[arg(long)]
        json: bool,
    },
}

impl Command {
    pub fn run(
        self,
        ledger: Option<&crate::scratch_ledger::Config>,
        context: &crate::scope::ExecutionContext,
    ) -> Result<()> {
        let Self::Remove {
            closeout,
            apply,
            chaosbox_allocation,
            json,
        } = self;
        ensure!(
            cfg!(target_os = "linux"),
            "worktree removal requires Linux process and mount inspection"
        );
        let (document, bytes, closeout) = load(&closeout)?;
        context.admit_tree(&document.worktree)?;
        context.admit_tree(&document.owning_repository)?;
        context.admit_path(&closeout)?;
        let digest = digest_bytes(&bytes)?;
        let (owner, mut limited_process_inspection) = validate_checkout(&document, context)?;
        verify_artifacts(&document, &closeout)?;
        let mut chaosbox_link = if let Some(allocation) = chaosbox_allocation {
            let ledger =
                ledger.context("--chaosbox-allocation requires Chaosbox scratch configuration")?;
            let allocation = exact_path(&allocation)?;
            ledger.preservation_allocation(&allocation, &document.worktree)?;
            Some(PreservationLink {
                allocation,
                reference: text(&closeout)?.to_owned(),
                description: format!(
                    "Doty verified worktree closeout; sha256={digest}; candidate={}; branch={}; handoff={}. Removal readiness only; pending obligations remain in the handoff.",
                    document.candidate,
                    document.branch,
                    document.handoff.display()
                ),
                recorded: false,
            })
        } else {
            None
        };
        if apply {
            context.admit_tree(&document.worktree)?;
            context.admit_tree(&document.owning_repository)?;
            if let Some(link) = &mut chaosbox_link {
                ledger
                    .context("Chaosbox configuration missing")?
                    .link_preservation(
                        &link.allocation,
                        &document.worktree,
                        &link.reference,
                        &link.description,
                    )?;
                link.recorded = true;
            }
            // Re-admit mutable source and manifest immediately before native removal.
            ensure!(
                load(&closeout)?.1 == bytes,
                "closeout changed during verification"
            );
            let (current_owner, omissions) = validate_checkout(&document, context)?;
            ensure!(current_owner == owner, "owning repository changed");
            limited_process_inspection.extend(omissions);
            limited_process_inspection.sort_unstable();
            limited_process_inspection.dedup();
            verify_artifacts(&document, &closeout)?;
            context.admit_tree(&document.worktree)?;
            git(
                &owner,
                &["worktree", "remove", "--", text(&document.worktree)?],
            )?;
            ensure!(
                !document.worktree.try_exists()?,
                "Git reported success but worktree remains"
            );
            ensure!(
                !registrations(&owner)?
                    .iter()
                    .any(|entry| entry.path == document.worktree),
                "worktree registration remains after removal"
            );
            ensure!(
                branch_head(&owner, &document.branch)? == document.candidate,
                "retained branch changed during removal"
            );
        }
        let result = Report {
            version: 1,
            closeout,
            closeout_sha256: digest,
            worktree: document.worktree,
            owning_repository: owner,
            candidate: document.candidate,
            branch: document.branch,
            verified_artifacts: document.artifacts.len(),
            removed: apply,
            limited_process_inspection,
            chaosbox_link,
        };
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!(
                "{} {} (HEAD {}, branch {})",
                if apply { "Removed" } else { "Ready to remove" },
                result.worktree.display(),
                result.candidate,
                result.branch
            );
            println!(
                "Verified {} external artifacts; closeout SHA-256 {}",
                result.verified_artifacts, result.closeout_sha256
            );
            if !result.limited_process_inspection.is_empty() {
                println!(
                    "Process inspection limited by /proc permissions for PIDs: {:?}",
                    result.limited_process_inspection
                );
            }
            if let Some(link) = result.chaosbox_link {
                println!(
                    "Chaosbox preservation link {} for {}",
                    if link.recorded { "recorded" } else { "planned" },
                    link.allocation.display()
                );
            }
            if !apply {
                println!("Preview only. Repeat with --apply to remove.");
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Closeout {
    ready_for_removal: bool,
    worktree: PathBuf,
    owning_repository: PathBuf,
    candidate: String,
    candidate_tree: String,
    branch: String,
    handoff: PathBuf,
    artifacts: BTreeMap<String, Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    sha256: String,
    bytes: u64,
}

#[derive(Serialize)]
struct Report {
    version: u32,
    closeout: PathBuf,
    closeout_sha256: String,
    worktree: PathBuf,
    owning_repository: PathBuf,
    candidate: String,
    branch: String,
    verified_artifacts: usize,
    removed: bool,
    limited_process_inspection: Vec<u32>,
    chaosbox_link: Option<PreservationLink>,
}

#[derive(Serialize)]
struct PreservationLink {
    allocation: PathBuf,
    reference: String,
    description: String,
    recorded: bool,
}

fn text(path: &Path) -> Result<&str> {
    path.to_str().context("worktree paths must be UTF-8")
}

fn exact_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "expected absolute path: {}",
        path.display()
    );
    let mut current = PathBuf::new();
    for part in path.components() {
        ensure!(
            !matches!(part, Component::ParentDir | Component::CurDir),
            "noncanonical path"
        );
        current.push(part.as_os_str());
        ensure!(
            !fs::symlink_metadata(&current)?.file_type().is_symlink(),
            "symlink component: {}",
            current.display()
        );
    }
    ensure!(
        path.canonicalize()? == path,
        "noncanonical path: {}",
        path.display()
    );
    Ok(path.to_owned())
}

fn regular(path: &Path) -> Result<File> {
    exact_path(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "not a regular artifact: {}",
        path.display()
    );
    Ok(file)
}

fn load(path: &Path) -> Result<(Closeout, Vec<u8>, PathBuf)> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let file = regular(&path)?;
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "closeout exceeds 4 MiB");
    let document: Closeout = serde_json::from_slice(&bytes).context("invalid closeout manifest")?;
    ensure!(
        document.ready_for_removal,
        "closeout is not readyForRemoval"
    );
    Ok((document, bytes, path))
}

fn digest_file(path: &Path) -> Result<String> {
    let file = regular(path)?;
    let before = crate::guard::identity_of(&file.metadata()?);
    let output = Process::new("sha256sum")
        .arg("-")
        .stdin(Stdio::from(file.try_clone()?))
        .output()
        .context("cannot execute sha256sum")?;
    ensure!(
        output.status.success(),
        "artifact checksum failed: {}",
        path.display()
    );
    ensure!(
        crate::guard::identity_of(&file.metadata()?) == before,
        "artifact changed while hashing: {}",
        path.display()
    );
    ensure!(
        crate::guard::identity_of(&fs::symlink_metadata(path)?) == before,
        "artifact replaced while hashing: {}",
        path.display()
    );
    parse_digest(output.stdout)
}

fn digest_bytes(bytes: &[u8]) -> Result<String> {
    let mut child = Process::new("sha256sum")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("checksum stdin missing")?
        .write_all(bytes)?;
    let output = child.wait_with_output()?;
    ensure!(output.status.success(), "closeout checksum failed");
    parse_digest(output.stdout)
}

fn parse_digest(bytes: Vec<u8>) -> Result<String> {
    let digest = String::from_utf8(bytes)?
        .split_whitespace()
        .next()
        .context("missing SHA-256")?
        .to_owned();
    ensure!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid SHA-256 output"
    );
    Ok(digest)
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Process::new("git");
    // A caller's unrelated Git environment must not redirect checkout inspection.
    for name in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(name);
    }
    let output = command
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-C"])
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("cannot execute Git")?;
    ensure!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(repo, args)?)?.trim().to_owned())
}

#[derive(Default)]
struct Registration {
    path: PathBuf,
    head: String,
    branch: String,
    blocked: bool,
}

fn registrations(repo: &Path) -> Result<Vec<Registration>> {
    let output = git(repo, &["worktree", "list", "--porcelain", "-z"])?;
    let mut entries = Vec::new();
    let mut entry = Registration::default();
    for field in output.split(|b| *b == 0) {
        let field = std::str::from_utf8(field)?;
        if field.is_empty() {
            if !entry.path.as_os_str().is_empty() {
                entries.push(std::mem::take(&mut entry));
            }
        } else if let Some(path) = field.strip_prefix("worktree ") {
            entry.path = path.into();
        } else if let Some(head) = field.strip_prefix("HEAD ") {
            entry.head = head.into();
        } else if let Some(branch) = field.strip_prefix("branch ") {
            entry.branch = branch.into();
        } else if field == "locked"
            || field.starts_with("locked ")
            || field == "prunable"
            || field.starts_with("prunable ")
            || field == "bare"
        {
            entry.blocked = true;
        }
    }
    Ok(entries)
}

fn branch_head(repo: &Path, branch: &str) -> Result<String> {
    let reference = format!("refs/heads/{branch}");
    git(repo, &["check-ref-format", &reference])?;
    git_text(repo, &["rev-parse", "--verify", &reference])
}

fn validate_checkout(
    doc: &Closeout,
    context: &crate::scope::ExecutionContext,
) -> Result<(PathBuf, Vec<u32>)> {
    let worktree = exact_path(&doc.worktree)?;
    ensure!(
        fs::symlink_metadata(worktree.join(".git"))?.is_file(),
        "not a linked worktree"
    );
    let common = git_text(
        &worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    context.admit_tree(Path::new(&common))?;
    let entries = registrations(&worktree)?;
    let owner = entries
        .first()
        .context("owning repository registration missing")?
        .path
        .clone();
    ensure!(
        exact_path(&owner)? == exact_path(&doc.owning_repository)?,
        "owning repository mismatch"
    );
    ensure!(worktree != owner, "cannot remove the owning repository");
    ensure!(
        git_text(
            &owner,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"]
        )? == common,
        "Git common directory mismatch"
    );
    let entry = entries
        .iter()
        .find(|entry| entry.path == worktree)
        .context("worktree is not registered")?;
    ensure!(!entry.blocked, "worktree is locked or prunable");
    ensure!(
        entry.head == doc.candidate
            && git_text(&worktree, &["rev-parse", "HEAD"])? == doc.candidate,
        "worktree HEAD differs from closeout candidate"
    );
    ensure!(
        entry.branch == format!("refs/heads/{}", doc.branch)
            && branch_head(&owner, &doc.branch)? == doc.candidate,
        "retained branch differs from closeout"
    );
    ensure!(
        git_text(&worktree, &["rev-parse", "HEAD^{tree}"])? == doc.candidate_tree,
        "candidate tree mismatch"
    );
    ensure!(
        git(
            &worktree,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none"
            ]
        )?
        .is_empty(),
        "worktree is dirty; finalize new changes first"
    );
    let admin = PathBuf::from(git_text(&worktree, &["rev-parse", "--absolute-git-dir"])?);
    for name in [
        "index.lock",
        "HEAD.lock",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
        "sequencer",
        "BISECT_LOG",
        "BISECT_HEAD",
        "BISECT_START",
    ] {
        ensure!(
            !admin.join(name).try_exists()?,
            "active Git operation: {name}"
        );
    }
    crate::guard::guard_registered_worktree(&worktree)?;
    for ancestor in worktree.ancestors() {
        ensure!(
            !marker_present(&ancestor.join(crate::guard::PROTECT_MARKER))?,
            "protected worktree ancestor: {}",
            ancestor.display()
        );
    }
    check_tree(&worktree)?;
    let omissions = check_workers(&worktree)?;
    Ok((owner, omissions))
}

fn verify_artifacts(doc: &Closeout, closeout: &Path) -> Result<()> {
    ensure!(
        !doc.handoff.starts_with(&doc.worktree),
        "artifact must be outside the worktree: {}",
        doc.handoff.display()
    );
    ensure!(
        !closeout.starts_with(&doc.worktree),
        "closeout must be outside the worktree"
    );
    let parent = closeout.parent().context("closeout parent missing")?;
    ensure!(
        !parent.starts_with(&doc.worktree),
        "evidence must be external"
    );
    let mut handoff_verified = false;
    for (name, artifact) in &doc.artifacts {
        let relative = Path::new(name);
        ensure!(
            !name.is_empty()
                && relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "artifact names must be relative paths without .."
        );
        let path = parent.join(relative);
        ensure!(
            !path.starts_with(&doc.worktree),
            "artifact must be outside the worktree: {}",
            path.display()
        );
        let file = regular(&path).with_context(|| format!("artifact {}", path.display()))?;
        ensure!(
            file.metadata()?.len() == artifact.bytes && digest_file(&path)? == artifact.sha256,
            "artifact identity/hash mismatch: {}",
            path.display()
        );
        handoff_verified |= path == doc.handoff;
    }
    ensure!(
        handoff_verified,
        "handoff must be included in external artifact inventory"
    );
    ensure!(
        doc.artifacts.contains_key("source.bundle"),
        "source.bundle must be included in artifact inventory"
    );
    let bundle = parent.join("source.bundle");
    // A preservation bundle must be standalone, rather than require local history.
    let mut header = BufReader::new(regular(&bundle)?);
    let mut remaining = 64 * 1024usize;
    loop {
        let mut line = String::new();
        let count = header
            .by_ref()
            .take((remaining + 1) as u64)
            .read_line(&mut line)?;
        ensure!(
            count > 0 && count <= remaining,
            "invalid or oversized bundle header"
        );
        remaining -= count;
        ensure!(
            !line.starts_with('-'),
            "source bundle has history prerequisites"
        );
        if line == "\n" {
            break;
        }
    }
    git(
        &doc.owning_repository,
        &["bundle", "verify", text(&bundle)?],
    )?;
    let heads = git_text(
        &doc.owning_repository,
        &["bundle", "list-heads", text(&bundle)?],
    )?;
    ensure!(
        heads
            .lines()
            .any(|line| line == format!("{} refs/heads/{}", doc.candidate, doc.branch)),
        "source bundle does not preserve the candidate branch"
    );
    Ok(())
}

fn check_tree(worktree: &Path) -> Result<()> {
    // Bind mounts can share st_dev. Inspect Linux's mount table before walking.
    for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let mount = line
            .split_whitespace()
            .nth(4)
            .context("invalid mountinfo")?;
        let mount = mount
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        ensure!(
            !Path::new(&mount).starts_with(worktree),
            "mounted path in worktree: {mount}"
        );
    }
    let mut remaining = 1_000_000usize;
    let device = fs::metadata(worktree)?.dev();
    let mut pending = vec![worktree.to_owned()];
    while let Some(dir) = pending.pop() {
        if dir != worktree {
            ensure!(
                !marker_present(&dir.join(".git"))?,
                "nested Git repository in worktree: {}",
                dir.display()
            );
        }
        ensure!(
            !marker_present(&dir.join(crate::guard::PROTECT_MARKER))?,
            "protected worktree subtree: {}",
            dir.display()
        );
        for item in fs::read_dir(&dir)? {
            ensure!(
                remaining > 0,
                "worktree inspection exceeds one million entries"
            );
            remaining -= 1;
            let path = item?.path();
            let meta = fs::symlink_metadata(&path)?;
            ensure!(
                meta.dev() == device,
                "mount boundary inside worktree: {}",
                path.display()
            );
            ensure!(
                meta.is_file() || meta.is_dir() || meta.file_type().is_symlink(),
                "special file in worktree: {}",
                path.display()
            );
            if meta.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn marker_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
fn check_workers(worktree: &Path) -> Result<Vec<u32>> {
    let uid = fs::metadata("/proc/self")?.uid();
    // /proc/maps escapes newlines as \\012, but leaves literal backslashes
    // unchanged. Compare in that representation to preserve mixed names and
    // conservatively protect the kernel's ambiguous newline/literal spelling.
    let maps_worktree = crate::process_path::maps_spelling(worktree);
    let mut omissions = Vec::new();
    let mut remaining = 1_000_000usize;
    for process in fs::read_dir("/proc")? {
        let process = process?;
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let path = process.path();
        let meta = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if uid != 0 && meta.uid() != uid {
            continue;
        }
        let mut limited = false;
        let inspection = (|| -> Result<()> {
            // Command arguments remain readable for many non-dumpable services.
            let mut command = Vec::new();
            File::open(path.join("cmdline"))?
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut command)?;
            ensure!(
                command.len() <= 4 * 1024 * 1024,
                "process command exceeds inspection budget"
            );
            let needle = text(worktree)?.as_bytes();
            ensure!(
                pid == std::process::id()
                    || !command
                        .windows(needle.len())
                        .enumerate()
                        .any(|(i, bytes)| bytes == needle
                            && command.get(i + needle.len()).is_none_or(|b| b
                                .is_ascii_whitespace()
                                || matches!(
                                    b,
                                    b'/' | 0
                                        | b'\''
                                        | b'"'
                                        | b':'
                                        | b','
                                        | b';'
                                        | b')'
                                        | b']'
                                        | b'}'
                                        | b'='
                                ))),
                "active worker {pid} command references the worktree"
            );
            for link in [path.join("cwd"), path.join("exe")] {
                limited |= check_process_link(&link, worktree)?;
            }
            match fs::read_dir(path.join("fd")) {
                Ok(fds) => {
                    for fd in fds {
                        ensure!(
                            remaining > 0,
                            "process descriptor inspection budget exhausted"
                        );
                        remaining -= 1;
                        limited |= check_process_link(&fd?.path(), worktree)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    limited = true
                }
                Err(error) => return Err(error.into()),
            }
            let maps = match File::open(path.join("maps")) {
                Ok(file) => {
                    let mut maps = String::new();
                    file.take(16 * 1024 * 1024 + 1).read_to_string(&mut maps)?;
                    ensure!(
                        maps.len() <= 16 * 1024 * 1024,
                        "process maps inspection budget exhausted"
                    );
                    maps
                }
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    limited = true;
                    String::new()
                }
                Err(error) => return Err(error.into()),
            };
            for line in maps.lines() {
                if let Some(offset) = line.find('/') {
                    ensure!(
                        !Path::new(line[offset..].trim_end_matches(" (deleted)"))
                            .starts_with(&maps_worktree),
                        "active worker {} maps worktree content",
                        process.file_name().to_string_lossy()
                    );
                }
            }
            Ok(())
        })();
        if let Err(error) = inspection {
            if !path.try_exists()? {
                continue;
            }
            // Zombies have no cwd/exe; they cannot use the checkout.
            if fs::read_to_string(path.join("stat"))
                .ok()
                .is_some_and(|stat| {
                    stat.rsplit_once(") ")
                        .is_some_and(|(_, rest)| rest.starts_with("Z "))
                })
            {
                continue;
            }
            return Err(error).with_context(|| {
                format!(
                    "cannot clear active worker {}",
                    process.file_name().to_string_lossy()
                )
            });
        }
        if limited {
            omissions.push(pid);
        }
    }
    Ok(omissions)
}

#[cfg(not(target_os = "linux"))]
fn check_workers(_: &Path) -> Result<Vec<u32>> {
    anyhow::bail!("worktree removal requires Linux process and mount inspection")
}

fn check_process_link(link: &Path, worktree: &Path) -> Result<bool> {
    match fs::read_link(link) {
        Ok(target) => ensure!(
            !Path::new(
                target
                    .to_str()
                    .context("non-UTF8 process reference")?
                    .trim_end_matches(" (deleted)")
            )
            .starts_with(worktree),
            "active worker reference {}",
            link.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(true),
        Err(error) => return Err(error.into()),
    }
    Ok(false)
}
