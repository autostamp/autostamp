//! Development task runner for the workspace.
//!
//! Run with `cargo xtask <task>`.

use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Workspace development tasks")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Run the full CI suite: fmt check, clippy, build, and tests.
    Ci,
    /// Format all crates in the workspace.
    Fmt,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.task {
        Task::Ci => ci(),
        Task::Fmt => fmt(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn ci() -> Result<(), String> {
    cargo(&["fmt", "--all", "--", "--check"])?;
    cargo(&[
        "clippy",
        "--all-targets",
        "--all-features",
        "--",
        "-Dwarnings",
    ])?;
    cargo(&["build", "--all", "--all-targets"])?;
    cargo(&["test", "--all"])?;
    Ok(())
}

fn fmt() -> Result<(), String> {
    cargo(&["fmt", "--all"])
}

fn cargo(args: &[&str]) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .args(args)
        .status()
        .map_err(|err| format!("failed to run `cargo {}`: {err}", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`cargo {}` failed", args.join(" ")))
    }
}
