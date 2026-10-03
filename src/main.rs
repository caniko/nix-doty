mod analyze;
mod cli;
mod commands;
mod config;
mod exec;
pub mod framework;
pub mod guard;
mod model_lock;
pub mod mount;
mod nix_ledger;
pub mod reclaim;
pub mod registry;
pub mod rm;
mod scratch_ledger;
pub mod targets;

fn main() -> anyhow::Result<()> {
    let cli = <cli::Cli as clap::Parser>::parse();
    cli.run()
}
