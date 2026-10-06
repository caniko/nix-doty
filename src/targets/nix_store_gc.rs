use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, ReclaimResult, Tier, Variant};
use crate::reclaim::runtime::{ActionContext, CommandOutput, Limits, Runtime, run_command};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::io::IsTerminal;

struct NixStoreGcFramework;

impl Framework for NixStoreGcFramework {
    fn name(&self) -> &'static str {
        "nix-store-gc"
    }
    fn summary(&self) -> &'static str {
        "Nix store garbage collection and optimisation"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&NhClean, &NixCollectGarbage, &Optimise]
    }
}

static FRAMEWORK: NixStoreGcFramework = NixStoreGcFramework;

pub static NIX_STORE_GC: &dyn Framework = &FRAMEWORK;

struct NhClean;
impl Variant for NhClean {
    fn name(&self) -> &'static str {
        "nh-clean"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Null)
    }
    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let settings = gc_settings(settings)?;
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/nix/store".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: u64::from(exec::path_exists("/nix/store")),
            notes: format!(
                "prune {} profiles once (keep {}, keep-since {}); preserve gcroots and direnv roots; bounded GC yield unknown",
                settings.scope.name(),
                settings.keep,
                settings.keep_since
            ),
        })
    }
    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, force, &Value::Null)
    }
    fn apply_with_settings(
        &self,
        apply: bool,
        force: bool,
        settings: &Value,
    ) -> Result<ApplyReport> {
        gc_settings(settings)?;
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec![],
            });
        }
        let limits = Limits::default();
        let runtime = Runtime::new(&limits);
        let context = ActionContext {
            mount: None,
            required_available_bytes: None,
            limits: &limits,
            runtime: &runtime,
        };
        let result = self.reclaim_with_settings(force, settings, &context)?;
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 1,
            freed_bytes: result.freed_bytes.unwrap_or(0),
            skipped: 0,
            errors: result.errors,
        })
    }
    fn reclaim_with_settings(
        &self,
        _force: bool,
        settings: &Value,
        context: &ActionContext<'_>,
    ) -> Result<ReclaimResult> {
        reclaim_nh(
            &gc_settings(settings)?,
            context,
            effective_uid() == 0,
            std::io::stdin().is_terminal(),
            |args| run_command(args, context.runtime.deadline),
            || context.goal_met(),
        )
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Scope {
    #[default]
    All,
    User,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::User => "user",
        }
    }
}

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct GcSettings {
    scope: Scope,
    keep: u32,
    keep_since: String,
}

impl Default for GcSettings {
    fn default() -> Self {
        Self {
            scope: Scope::All,
            keep: 3,
            keep_since: "14d".into(),
        }
    }
}

fn gc_settings(value: &Value) -> Result<GcSettings> {
    let settings: GcSettings = if value.is_null() {
        GcSettings::default()
    } else {
        serde_json::from_value(value.clone())?
    };
    anyhow::ensure!(
        settings.keep > 0 && !settings.keep_since.is_empty(),
        "Nix retention must keep at least one generation and specify keepSince"
    );
    Ok(settings)
}

fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid has no arguments or memory preconditions.
    unsafe { libc::geteuid() }
}

fn empty_result() -> ReclaimResult {
    ReclaimResult {
        freed_bytes: None,
        notices: Vec::new(),
        errors: Vec::new(),
        command_log: String::new(),
    }
}

fn record_command(
    result: &mut ReclaimResult,
    args: &[&str],
    run: &mut impl FnMut(&[&str]) -> Result<CommandOutput>,
) -> Option<CommandOutput> {
    match run(args) {
        Ok(output) => {
            result.command_log.push_str(&format!(
                "$ {}\n{}{}\n",
                args.join(" "),
                output.stdout,
                output.stderr
            ));
            // Retain a bounded UTF-8 tail across all passes, in addition to live stderr progress.
            if result.command_log.len() > 65536 {
                let mut cut = result.command_log.len() - 65536;
                while !result.command_log.is_char_boundary(cut) {
                    cut += 1;
                }
                result.command_log.drain(..cut);
            }
            Some(output)
        }
        Err(error) => {
            result.errors.push(format!("{error:#}"));
            None
        }
    }
}

fn reclaim_nh(
    settings: &GcSettings,
    context: &ActionContext<'_>,
    root: bool,
    interactive: bool,
    mut run: impl FnMut(&[&str]) -> Result<CommandOutput>,
    mut goal_met: impl FnMut() -> Result<bool>,
) -> Result<ReclaimResult> {
    let mut result = empty_result();
    if goal_met()? {
        result
            .notices
            .push("free-space goal already reached".into());
        return Ok(result);
    }
    let elevate = matches!(settings.scope, Scope::All) && !root;
    if elevate {
        let args: &[&str] = if interactive {
            &["sudo", "-v"]
        } else {
            &["sudo", "-n", "-v"]
        };
        if record_command(&mut result, args, &mut run).is_none() {
            return Ok(result);
        }
    }
    if context.runtime.remaining_gc_bytes.get() == 0 {
        result
            .notices
            .push("Nix logical-byte GC budget exhausted".into());
        return Ok(result);
    }
    let keep = settings.keep.to_string();
    let mut prune = if elevate {
        vec!["sudo", "-n", "--"]
    } else {
        vec![]
    };
    prune.extend([
        "nh",
        "clean",
        settings.scope.name(),
        "--keep",
        &keep,
        "--keep-since",
        &settings.keep_since,
        "--no-gc",
        "--no-gcroots",
        "--no-direnv",
        "--elevation-strategy",
        "none",
    ]);
    if record_command(&mut result, &prune, &mut run).is_none() {
        return Ok(result);
    }
    result.notices.push(format!("profile retention applied once: keep {}, keep-since {}; gcroots and direnv roots preserved", settings.keep, settings.keep_since));
    // Profile changes may free space or overlap another writer's cleanup.
    if !goal_met()? {
        collect_bounded(&mut result, context, elevate, &mut run, &mut goal_met)?;
    }
    Ok(result)
}

fn collect_bounded(
    result: &mut ReclaimResult,
    context: &ActionContext<'_>,
    elevate: bool,
    run: &mut impl FnMut(&[&str]) -> Result<CommandOutput>,
    goal_met: &mut impl FnMut() -> Result<bool>,
) -> Result<()> {
    let mut freed = 0u64;
    loop {
        if goal_met()? {
            result.notices.push("free-space goal reached".into());
            break;
        }
        if let Some(reason) = context.runtime.stop_reason() {
            result.errors.push(reason.into());
            break;
        }
        let limit = context
            .limits
            .gc_pass_bytes
            .min(context.runtime.remaining_gc_bytes.get());
        if limit == 0 {
            result
                .notices
                .push("Nix logical-byte GC budget exhausted".into());
            break;
        }
        let limit_string = limit.to_string();
        let mut args = if elevate {
            vec!["sudo", "-n", "--"]
        } else {
            vec![]
        };
        args.extend(["nix-store", "--gc", "--max-freed", &limit_string]);
        let Some(output) = record_command(result, &args, run) else {
            break;
        };
        let bytes = parse_freed_bytes(&output.stdout).or_else(|| parse_freed_bytes(&output.stderr));
        let Some(bytes) = bytes else {
            result.notices.push("Nix yield unknown: no byte summary; stopped rather than repeating an unaccounted GC pass".into());
            result.freed_bytes = None;
            break;
        };
        freed = freed.saturating_add(bytes);
        result.freed_bytes = Some(freed);
        context.runtime.remaining_gc_bytes.set(
            context
                .runtime
                .remaining_gc_bytes
                .get()
                .saturating_sub(bytes),
        );
        if bytes < limit {
            result.notices.push("GC completed without reaching the pass limit; no further unreachable paths reported".into());
            break;
        }
    }
    result.notices.push("Nix reports logical bytes; net filesystem availability is measured separately; a pass can exceed its requested limit by the last deleted path".into());
    Ok(())
}

fn parse_freed_bytes(output: &str) -> Option<u64> {
    output.lines().rev().find_map(|line| {
        let prefix = line.split_once(" bytes freed")?.0;
        prefix.split_whitespace().last()?.parse().ok()
    })
}

struct NixCollectGarbage;
impl Variant for NixCollectGarbage {
    fn name(&self) -> &'static str {
        "nix-collect-garbage"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/nix/store".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: u64::from(exec::path_exists("/nix/store")),
            notes: "reclaim runs bounded GC preserving generations; standalone run deletes old generations".into(),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec![],
            });
        }
        exec::run_stdout(&["sudo", "nix-collect-garbage", "-d"])?;
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 1,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
    fn reclaim_with_settings(
        &self,
        _force: bool,
        _settings: &Value,
        context: &ActionContext<'_>,
    ) -> Result<ReclaimResult> {
        let mut result = empty_result();
        collect_bounded(
            &mut result,
            context,
            false,
            &mut |args| run_command(args, context.runtime.deadline),
            &mut || context.goal_met(),
        )?;
        Ok(result)
    }
}

struct Optimise;
impl Variant for Optimise {
    fn name(&self) -> &'static str {
        "optimise"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/nix/store".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: u64::from(exec::path_exists("/nix/store")),
            notes: "hardlinks identical store paths — reduces disk usage".into(),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec![],
            });
        }
        exec::run_stdout(&["nix", "store", "optimise"])?;
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 0,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
    fn reclaim_with_settings(
        &self,
        _force: bool,
        _settings: &Value,
        context: &ActionContext<'_>,
    ) -> Result<ReclaimResult> {
        let mut result = empty_result();
        record_command(&mut result, &["nix", "store", "optimise"], &mut |args| {
            run_command(args, context.runtime.deadline)
        });
        result
            .notices
            .push("optimisation yield unmeasured; use net filesystem availability".into());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(bytes: u64) -> CommandOutput {
        CommandOutput {
            stdout: String::new(),
            stderr: format!("2 store paths deleted, {bytes} bytes freed (1 MiB)\n"),
            ..Default::default()
        }
    }

    #[test]
    fn unavailable_sudo_prevents_profile_changes_and_gc() {
        let limits = Limits::default();
        let runtime = Runtime::new(&limits);
        let context = ActionContext {
            mount: None,
            required_available_bytes: None,
            limits: &limits,
            runtime: &runtime,
        };
        let mut commands = Vec::new();
        let result = reclaim_nh(
            &GcSettings::default(),
            &context,
            false,
            false,
            |args| {
                commands.push(args.join(" "));
                anyhow::bail!("sudo: a password is required")
            },
            || Ok(false),
        )
        .unwrap();
        assert_eq!(commands, ["sudo -n -v"]);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("password is required"));
    }

    #[test]
    fn retention_runs_once_and_gc_stops_at_measured_goal() {
        let limits = Limits {
            gc_pass_bytes: 100,
            gc_max_bytes: 1000,
            ..Default::default()
        };
        let runtime = Runtime::new(&limits);
        let context = ActionContext {
            mount: None,
            required_available_bytes: None,
            limits: &limits,
            runtime: &runtime,
        };
        let available = std::cell::Cell::new(0);
        let mut commands = Vec::new();
        let result = reclaim_nh(
            &GcSettings::default(),
            &context,
            true,
            false,
            |args| {
                commands.push(args.join(" "));
                if args[0] == "nix-store" {
                    available.set(available.get() + 60);
                }
                Ok(output(100))
            },
            || Ok(available.get() >= 120),
        )
        .unwrap();
        assert_eq!(commands.len(), 3);
        assert!(commands[0].contains("--no-gc --no-gcroots --no-direnv"));
        assert!(commands[0].contains("--keep 3 --keep-since 14d"));
        assert_eq!(result.freed_bytes, Some(200));
        assert!(result.errors.is_empty());
    }

    #[test]
    fn logical_budget_bounds_gc_despite_no_net_filesystem_gain() {
        let limits = Limits {
            gc_pass_bytes: 100,
            gc_max_bytes: 150,
            ..Default::default()
        };
        let runtime = Runtime::new(&limits);
        let context = ActionContext {
            mount: None,
            required_available_bytes: None,
            limits: &limits,
            runtime: &runtime,
        };
        let mut result = empty_result();
        let mut commands = Vec::new();
        collect_bounded(
            &mut result,
            &context,
            false,
            &mut |args| {
                commands.push(args.join(" "));
                Ok(output(args.last().unwrap().parse().unwrap()))
            },
            &mut || Ok(false),
        )
        .unwrap();
        assert_eq!(
            commands,
            [
                "nix-store --gc --max-freed 100",
                "nix-store --gc --max-freed 50"
            ]
        );
        assert_eq!(result.freed_bytes, Some(150));
        assert!(
            result
                .notices
                .iter()
                .any(|notice| notice.contains("budget exhausted"))
        );
    }

    #[test]
    fn unknown_yield_is_a_notice_and_does_not_repeat_gc() {
        let limits = Limits::default();
        let runtime = Runtime::new(&limits);
        let context = ActionContext {
            mount: None,
            required_available_bytes: None,
            limits: &limits,
            runtime: &runtime,
        };
        let mut result = empty_result();
        let mut count = 0;
        collect_bounded(
            &mut result,
            &context,
            false,
            &mut |_| {
                count += 1;
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: "done".into(),
                    ..Default::default()
                })
            },
            &mut || Ok(false),
        )
        .unwrap();
        assert_eq!(count, 1);
        assert!(result.freed_bytes.is_none());
        assert!(result.errors.is_empty());
        assert!(result.notices[0].contains("yield unknown"));
    }
}
