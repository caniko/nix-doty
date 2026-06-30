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
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
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
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Reclaim disk space on full mounts (dry-run by default)
    Reclaim {
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
        match self.command {
            Command::List { json } => crate::commands::list(json),
            Command::Status {
                target,
                variant,
                config,
                json,
            } => crate::commands::status(target, variant, &config, json),
            Command::Run {
                target,
                variant,
                config,
                apply,
                force,
                json,
            } => crate::commands::run(target, variant, &config, apply, force, json),
            Command::Doctor { config, json } => crate::commands::doctor(&config, json),
            Command::Reclaim {
                mount,
                threshold,
                min_free_bytes,
                min_free_pct,
                apply,
                force,
                all,
                json,
            } => crate::commands::reclaim(
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
