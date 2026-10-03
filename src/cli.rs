use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "doty",
    version,
    about = "Do That Yourself: NixOS cleanup orchestrator",
    long_about = "doty inspects and cleans temporary state accumulated by NixOS services and frameworks. Each framework has one or more cleanup variants with tiered safety (safe, confirm, risky, report-only). Run `doty status` to inspect, `doty run` to act. Dry-run by default."
)]
pub struct Cli {
    #[command(flatten)]
    ledger: crate::scratch_ledger::Options,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Inspect tracked Nix objects and their reasons; read-only, never releases roots or runs GC
    NixStore {
        /// Exact store path, to inspect all recorded consumers
        #[arg(long)]
        path: Option<String>,
        /// Bounded journal pagination cursor
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Analyze agent scratch space without reading contents or deleting anything
    Analyze {
        #[command(subcommand)]
        agent: crate::analyze::Agent,
    },
    /// List all available cleanup frameworks and their variants
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Inspect what would be cleaned (read-only, dry-run)
    Status {
        /// Target framework name
        #[arg(short, long, value_name = "FRAMEWORK")]
        target: Option<String>,
        /// Variant name
        #[arg(short, long, value_name = "VARIANT")]
        variant: Option<String>,
        /// Path to configured targets.json
        #[arg(long, default_value = crate::config::DEFAULT_CONFIG_PATH)]
        config: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run cleanup (dry-run by default, use --apply to mutate)
    Run {
        /// Target framework name
        #[arg(short, long, value_name = "FRAMEWORK")]
        target: Option<String>,
        /// Variant name
        #[arg(short, long, value_name = "VARIANT")]
        variant: Option<String>,
        /// Path to configured targets.json
        #[arg(long, default_value = crate::config::DEFAULT_CONFIG_PATH)]
        config: String,
        /// Actually perform cleanup (default is dry-run)
        #[arg(long)]
        apply: bool,
        /// Bypass tier gating for confirm/risky targets
        #[arg(long)]
        force: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Compare installed targets against NixOS-declared config
    Doctor {
        /// Path to the NixOS-declared targets.json
        #[arg(long, default_value = "/etc/doty/targets.json")]
        config: String,
        /// Compare activated configuration to an evaluated candidate document
        #[arg(long)]
        expected_config: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove scratch paths via a guarded plan (dry-run preview by default)
    Rm {
        /// Allowed root all targets must live under
        #[arg(long, default_value = crate::rm::DEFAULT_SCRATCH_ROOT)]
        root: String,
        /// Actually quarantine (default only plans)
        #[arg(long)]
        apply: bool,
        /// Apply a previously created plan by id
        #[arg(long, value_name = "PLAN_ID")]
        plan: Option<String>,
        /// Exact target paths (required unless --plan is given)
        #[arg(last = true, value_name = "PATH")]
        targets: Vec<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Restore quarantined entries from a removal plan
    Restore {
        /// Removal plan id to restore
        #[arg(value_name = "PLAN_ID")]
        plan: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Permanently purge quarantined entries (separate approval required)
    Purge {
        /// Removal plan id to purge
        #[arg(value_name = "PLAN_ID")]
        plan: String,
        /// Actually purge (default only previews)
        #[arg(long)]
        apply: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Reclaim disk space on full mounts (dry-run by default)
    Reclaim {
        /// Path to configured targets.json
        #[arg(long, default_value = crate::config::DEFAULT_CONFIG_PATH)]
        config: String,
        /// Only plan/reclaim for specific mount point
        #[arg(short, long, value_name = "MOUNT")]
        mount: Option<String>,
        /// Usage threshold percentage (default: 85)
        #[arg(long, default_value = "85.0")]
        threshold: f64,
        /// Minimum free space target in bytes (e.g. "1073741824" for 1 GiB)
        #[arg(long, value_name = "BYTES")]
        min_free_bytes: Option<u64>,
        /// Minimum free space target as percentage
        #[arg(long, value_name = "PCT")]
        min_free_pct: Option<f64>,
        /// Actually perform reclamation (default is dry-run)
        #[arg(long)]
        apply: bool,
        /// Bypass tier gating for confirm/risky targets
        #[arg(long)]
        force: bool,
        /// Include all mounts, not just those above threshold
        #[arg(long)]
        all: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

impl Cli {
    pub fn run(self) -> Result<()> {
        let ledger = self.ledger.config()?;
        match self.command {
            Command::NixStore { path, offset } => {
                let result = crate::nix_ledger::inspect(path.as_deref(), offset)?;
                println!("{}", serde_json::to_string_pretty(&result)?);
                Ok(())
            }
            Command::Analyze { agent } => agent.run(ledger.as_ref()),
            Command::List { json } => crate::commands::list(json),
            Command::Status {
                target,
                variant,
                config,
                json,
            } => crate::commands::status(target, variant, &config, json, ledger.as_ref()),
            Command::Run {
                target,
                variant,
                config,
                apply,
                force,
                json,
            } => crate::commands::run(
                target,
                variant,
                &config,
                apply,
                force,
                json,
                ledger.as_ref(),
            ),
            Command::Doctor {
                config,
                expected_config,
                json,
            } => crate::commands::doctor(&config, expected_config.as_deref(), json),
            Command::Rm {
                root,
                apply,
                plan,
                targets,
                json,
            } => crate::commands::rm(&root, apply, plan.as_deref(), &targets, json, ledger),
            Command::Restore { plan, json } => crate::commands::restore(&plan, json),
            Command::Purge { plan, apply, json } => crate::commands::purge(&plan, apply, json),
            Command::Reclaim {
                config,
                mount,
                threshold,
                min_free_bytes,
                min_free_pct,
                apply,
                force,
                all,
                json,
            } => crate::commands::reclaim(
                config,
                mount,
                threshold,
                min_free_bytes,
                min_free_pct,
                apply,
                force,
                all,
                json,
            ),
        }
    }
}
