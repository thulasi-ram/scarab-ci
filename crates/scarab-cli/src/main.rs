//! # scarab-cli — the `scarab` developer CLI.
//!
//! Compiling skeleton: each subcommand prints a "not yet implemented" line.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "scarab", about = "Scarab durable CI — developer CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Trigger a pipeline run.
    Run,
    /// Lint a pipeline file for style/anti-patterns.
    Lint,
    /// Validate a pipeline file (compile + semantic checks).
    Validate,
    /// Stream logs for a run or step.
    Logs,
    /// Restart a failed or completed run.
    Restart,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Run => println!("scarab run: not yet implemented"),
        Command::Lint => println!("scarab lint: not yet implemented"),
        Command::Validate => println!("scarab validate: not yet implemented"),
        Command::Logs => println!("scarab logs: not yet implemented"),
        Command::Restart => println!("scarab restart: not yet implemented"),
    }
}
