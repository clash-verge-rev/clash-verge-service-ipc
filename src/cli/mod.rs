use anyhow::Error;
use clap::{Parser, Subcommand};

pub mod run;
pub mod install;
pub mod uninstall;

#[derive(Parser, Debug)]
#[command(name = "clash-verge-service-cli", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run Service Binary
    Run,
    /// Install Service Binary to system
    Install,
    /// Uninstall Service Binary from system
    Uninstall,
}

pub fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Install => {
            if let Err(e) = install::main() {
                eprintln!("Installation failed: {}", e);
                return Err(e);
            }
            println!("Install Success");
        }
        Command::Uninstall => {
            if let Err(e) = uninstall::main() {
                eprintln!("Uninstallation failed: {}", e);
                return Err(e);
            }
            println!("Uninstall Success");
        }
        Command::Run => {
            if let Err(e) = run::main() {
                eprintln!("Service failed to run: {}", e);
                return Err(e);
            }
        }
    }
    Ok(())
}

pub fn run_command(cmd: &str, args: &[&str], debug: bool) -> Result<(), Error> {
    if debug {
        println!("Executing: {} {}", cmd, args.join(" "));
    }
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to execute '{}': {}", cmd, e))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if debug {
        eprintln!(
            "Command failed (status: {}):\nstdout: {}\nstderr: {}",
            output.status, stdout, stderr
        );
    }
    Err(anyhow::anyhow!(
        "Command '{}' failed (status: {}):\nstdout: {}\nstderr: {}",
        cmd,
        output.status,
        stdout,
        stderr
    ))
}
