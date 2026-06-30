mod cli;
mod commands;
mod config;
mod exec;
pub mod framework;
pub mod mount;
pub mod reclaim;
pub mod registry;
pub mod targets;

fn main() -> anyhow::Result<()> {
    let cli = <cli::Cli as clap::Parser>::parse();
    cli.run()
}
