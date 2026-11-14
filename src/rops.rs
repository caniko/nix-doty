//! Queue a system-mode mutation for root approval instead of running it.
//!
//! The queued step is argv, not shell: the absolute path of the running binary
//! plus the original arguments with `--rops` removed. Approval, sudo and host
//! lookup belong to the operator surface (`canix host rops`).

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::cli::Command;

/// Value-taking flags that name a path. A relative value would resolve in the
/// caller's working directory, which the approving root session need not share.
const PATH_FLAGS: &[&str] = &[
    "--config",
    "--root",
    "--mount",
    "--chaosbox-work",
    "--chaosbox-root",
    "--chaosbox-bin",
];

/// Validate the invocation and queue it instead of running it.
pub(crate) fn submit(
    mode: crate::scope::Mode,
    user_root: &[PathBuf],
    command: &Command,
) -> Result<()> {
    // SAFETY: identity query has no arguments or side effects.
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "--rops is for unprivileged callers; run without --rops as root"
    );
    ensure!(
        mode == crate::scope::Mode::System,
        "--rops requires --mode system"
    );
    ensure!(user_root.is_empty(), "--user-root requires user mode");
    let summary = summarize(command)?;
    let program = std::env::current_exe()
        .context("cannot resolve the running doty binary")?
        .canonicalize()
        .context("cannot canonicalize the running doty binary")?;
    ensure!(
        program.is_absolute(),
        "doty binary path is not absolute: {}",
        program.display()
    );
    let args: Vec<OsString> = std::env::args_os().collect();
    let request = request(command, &args, &program, hostname()?)?;
    let op = queue(&fleetix::rops::default_spool_dir(), request)?;
    announce(&op, summary.json)
}

fn queue(spool: &Path, request: fleetix::rops::Request) -> Result<fleetix::rops::Op> {
    fleetix::rops::queue(spool, request).map_err(|error| anyhow::anyhow!("{error}"))
}

fn announce(op: &fleetix::rops::Op, json: bool) -> Result<()> {
    if json {
        #[derive(Serialize)]
        struct Rops<'a> {
            id: &'a str,
            host: &'a str,
        }
        #[derive(Serialize)]
        struct Output<'a> {
            rops: Rops<'a>,
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&Output {
                rops: Rops {
                    id: &op.id,
                    host: &op.host,
                },
            })?
        );
    } else {
        println!(
            "Queued {} for {}. Approve with `canix host rops {}`; follow with `canix host rops watch {}`.",
            op.id, op.host, op.host, op.id
        );
    }
    Ok(())
}

#[derive(Debug)]
struct Summary {
    title: String,
    reason: String,
    json: bool,
}

/// Which mutations root can repeat without the caller's environment.
fn summarize(command: &Command) -> Result<Summary> {
    match command {
        Command::Run {
            target,
            variant,
            apply: true,
            json,
            ..
        } => {
            let selection = match (target.as_deref(), variant.as_deref()) {
                (Some(target), Some(variant)) => format!("{target}/{variant}"),
                (Some(target), None) => format!("{target} (all configured variants)"),
                (None, Some(variant)) => format!("configured {variant}"),
                (None, None) => "configured targets".to_owned(),
            };
            Ok(Summary {
                title: title(format!("doty run --apply {selection}")),
                reason: format!(
                    "Run system cleanup for {selection}, requested from the user session."
                ),
                json: *json,
            })
        }
        Command::Reclaim {
            apply: true,
            mount,
            json,
            ..
        } => {
            let surface = match mount {
                Some(mount) => format!("mount {}", absolute(mount)?),
                None => "mounts over threshold".to_owned(),
            };
            Ok(Summary {
                title: title(format!("doty reclaim --apply {surface}")),
                reason: format!(
                    "Run system reclaim of {surface}, requested from the user session."
                ),
                json: *json,
            })
        }
        Command::Rm {
            apply: true,
            plan: None,
            targets,
            json,
            ..
        } if !targets.is_empty() => {
            let paths = targets
                .iter()
                .map(|target| absolute(target))
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            Ok(Summary {
                title: title(format!("doty rm --apply {paths}")),
                reason: format!("Quarantine {paths}, requested from the user session."),
                json: *json,
            })
        }
        Command::Rm {
            apply: true,
            plan: Some(_),
            ..
        } => bail!(
            "--rops cannot queue `rm --plan`: removal plans live under the caller's XDG state directory, which the approving root session does not read; queue `rm --apply` with explicit paths"
        ),
        Command::Rm { .. } => {
            bail!("--rops applies only to mutating forms; pass `rm --apply` with explicit paths")
        }
        Command::Purge { .. } => bail!(
            "--rops cannot queue `purge`: removal plans live under the caller's XDG state directory, which the approving root session does not read; queue `rm --apply` and purge from the same approved session"
        ),
        Command::Run { .. } => bail!("--rops applies only to mutating forms; pass `run --apply`"),
        Command::Reclaim { .. } => {
            bail!("--rops applies only to mutating forms; pass `reclaim --apply`")
        }
        _ => bail!("--rops applies only to `run --apply`, `reclaim --apply` and `rm --apply`"),
    }
}

fn request(
    command: &Command,
    args: &[OsString],
    program: &Path,
    host: String,
) -> Result<fleetix::rops::Request> {
    let summary = summarize(command)?;
    Ok(fleetix::rops::Request {
        host,
        title: summary.title,
        reason: summary.reason,
        commands: vec![rewrite(args, command, program)?],
        requested_by: fleetix::rops::requester("doty"),
    })
}

/// Replace argv[0] with the absolute binary path, drop `--rops`, and make path
/// arguments absolute. `--rops` after `--` stays: there it is a target path.
fn rewrite(args: &[OsString], command: &Command, program: &Path) -> Result<Vec<String>> {
    let program = program
        .to_str()
        .context("doty binary path is not valid UTF-8")?;
    let mut argv = vec![program.to_owned()];
    let mut trailing = false;
    let mut index = 1;
    while index < args.len() {
        let arg = args[index]
            .to_str()
            .with_context(|| format!("argument {} is not valid UTF-8", index + 1))?;
        index += 1;
        if trailing {
            let trailing_path = matches!(command, Command::Rm { .. });
            argv.push(if trailing_path {
                absolute(arg)?
            } else {
                arg.to_owned()
            });
            continue;
        }
        if arg == "--" {
            trailing = true;
            argv.push(arg.to_owned());
            continue;
        }
        if arg == "--rops" {
            continue;
        }
        // `-m` is the only short path flag: `-m PATH`, `-mPATH`, `-m=PATH`.
        if let Some(attached) = arg.strip_prefix("-m") {
            let value = if attached.is_empty() {
                let value = args.get(index).context("-m requires a path value")?;
                index += 1;
                value.to_str().context("-m value is not valid UTF-8")?
            } else {
                attached.strip_prefix('=').unwrap_or(attached)
            };
            argv.push(format!("--mount={}", absolute(value)?));
            continue;
        }
        if let Some((flag, value)) = arg.split_once('=') {
            argv.push(if PATH_FLAGS.contains(&flag) {
                format!("{flag}={}", absolute(value)?)
            } else {
                arg.to_owned()
            });
            continue;
        }
        if PATH_FLAGS.contains(&arg) {
            let value = args
                .get(index)
                .with_context(|| format!("{arg} requires a path value"))?;
            index += 1;
            let value = value
                .to_str()
                .with_context(|| format!("{arg} value is not valid UTF-8"))?;
            argv.push(arg.to_owned());
            argv.push(absolute(value)?);
            continue;
        }
        argv.push(arg.to_owned());
    }
    Ok(argv)
}

/// Resolve a path against the caller's working directory without touching the
/// filesystem: the approving root session has a different cwd.
fn absolute(value: &str) -> Result<String> {
    let path = std::path::absolute(value)
        .with_context(|| format!("cannot make path absolute: {value:?}"))?;
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

/// One line for the approval summary; fleetix rejects non-printable characters.
fn title(text: String) -> String {
    text.chars().take(200).collect()
}

fn hostname() -> Result<String> {
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return Ok(name.to_owned());
        }
    }
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its full length and gethostname writes
    // at most that many bytes plus a terminator on Linux.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    ensure!(result == 0, "cannot read the local hostname");
    let name = std::ffi::CStr::from_bytes_until_nul(&buffer)
        .context("cannot read the local hostname")?
        .to_str()
        .context("local hostname is not valid UTF-8")?;
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(apply: bool) -> Command {
        Command::Run {
            target: None,
            variant: None,
            config: None,
            apply,
            force: false,
            json: false,
        }
    }

    fn rm(apply: bool, plan: Option<&str>, targets: &[&str]) -> Command {
        Command::Rm {
            root: None,
            apply,
            plan: plan.map(str::to_owned),
            targets: targets.iter().map(|target| (*target).to_owned()).collect(),
            json: false,
        }
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    const PROGRAM: &str = "/nix/store/0123-doty/bin/doty";

    #[test]
    fn rewrite_strips_rops_and_absolutizes_paths() {
        let argv = rewrite(
            &args(&[
                "doty",
                "--mode",
                "system",
                "--rops",
                "run",
                "--apply",
                "--config",
                "../conf/targets.json",
            ]),
            &run(true),
            Path::new(PROGRAM),
        )
        .unwrap();
        assert_eq!(argv[0], PROGRAM);
        assert!(Path::new(&argv[0]).is_absolute());
        assert!(!argv.iter().any(|arg| arg == "--rops"));
        assert_eq!(
            argv.last().unwrap(),
            &absolute("../conf/targets.json").unwrap()
        );
        assert!(Path::new(argv.last().unwrap()).is_absolute());

        // A target literally named `--rops` stays: there it is a path.
        let argv = rewrite(
            &args(&["doty", "--mode", "system", "rm", "--apply", "--", "--rops"]),
            &rm(true, None, &["--rops"]),
            Path::new(PROGRAM),
        )
        .unwrap();
        assert_eq!(argv.last().unwrap(), &absolute("--rops").unwrap());
    }

    #[test]
    fn every_mount_spelling_reaches_root_absolute() {
        use clap::Parser;
        let expected = absolute(".").unwrap();
        for mount in [
            ["-m", "."].as_slice(),
            &["-m."],
            &["-m=."],
            &["--mount", "."],
        ] {
            let mut values = vec!["doty", "--mode", "system", "--rops", "reclaim", "--apply"];
            values.extend_from_slice(mount);
            let cli = crate::cli::Cli::try_parse_from(&values).unwrap();
            let argv = rewrite(&args(&values), &cli.command, Path::new(PROGRAM)).unwrap();
            let queued = crate::cli::Cli::try_parse_from(&argv).unwrap();
            let Command::Reclaim { mount, .. } = queued.command else {
                panic!("not reclaim: {argv:?}");
            };
            assert_eq!(mount.as_deref(), Some(expected.as_str()), "{values:?}");
        }
    }

    #[test]
    fn clap_accepts_the_global_flag_in_every_position() {
        use clap::Parser;
        for values in [
            ["doty", "--rops", "--mode", "system", "run", "--apply"].as_slice(),
            ["doty", "--mode", "system", "run", "--rops", "--apply"].as_slice(),
            [
                "doty", "--mode", "system", "rm", "--apply", "--rops", "--", "a",
            ]
            .as_slice(),
        ] {
            let cli = crate::cli::Cli::try_parse_from(values).unwrap();
            assert!(cli.rops);
        }
    }

    #[test]
    fn non_mutating_forms_are_rejected() {
        assert!(summarize(&run(false)).is_err());
        assert!(summarize(&rm(false, None, &["a"])).is_err());
        let error = summarize(&rm(true, Some("rm-1"), &[])).unwrap_err();
        assert!(
            format!("{error}").contains("XDG state directory"),
            "{error}"
        );
        let error = summarize(&Command::Purge {
            plan: "rm-1".into(),
            apply: true,
            json: false,
        })
        .unwrap_err();
        assert!(
            format!("{error}").contains("XDG state directory"),
            "{error}"
        );
        assert!(
            summarize(&Command::Status {
                target: None,
                variant: None,
                config: None,
                json: false,
            })
            .is_err()
        );
    }

    #[test]
    fn queued_step_is_absolute_and_rops_free() {
        let spool = tempfile::tempdir().unwrap();
        let command = rm(
            true,
            None,
            &["scratch/stale", "/data/scratch/tmp/opencode/old"],
        );
        let request = request(
            &command,
            &args(&[
                "doty",
                "--mode",
                "system",
                "rm",
                "--apply",
                "--rops",
                "--",
                "scratch/stale",
                "/data/scratch/tmp/opencode/old",
            ]),
            Path::new(PROGRAM),
            "atlas".into(),
        )
        .unwrap();
        let op = fleetix::rops::queue(spool.path(), request).unwrap();
        let file = spool
            .path()
            .join(fleetix::rops::PENDING)
            .join(format!("{}.json", op.id));
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        println!("{}", serde_json::to_string_pretty(&value).unwrap());

        let step = value["commands"][0].as_array().unwrap();
        assert_eq!(step[0].as_str(), Some(PROGRAM));
        assert!(Path::new(step[0].as_str().unwrap()).is_absolute());
        assert!(!step.iter().any(|arg| arg.as_str() == Some("--rops")));
        assert_eq!(
            step.last().unwrap().as_str(),
            Some("/data/scratch/tmp/opencode/old")
        );
        assert!(step[6].as_str().unwrap().starts_with('/'));
        assert_eq!(value["host"], "atlas");
        assert!(value["requested_by"].as_str().unwrap().starts_with("doty:"));
    }
}
