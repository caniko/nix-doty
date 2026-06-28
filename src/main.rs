mod cli;
mod commands;
mod exec;
pub mod framework;
pub mod registry;
pub mod targets;

fn main() -> anyhow::Result<()> {
    let cli = <cli::Cli as clap::Parser>::parse();
    cli.run()
}
