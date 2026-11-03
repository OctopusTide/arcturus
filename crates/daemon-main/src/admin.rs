use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
#[derive(Parser)]
#[command(name=proto::names::ADMIN_BIN,version,about="Narrow Windows administrator service; installation is an explicit user action")]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Install from an elevated terminal into an explicitly selected protected local directory.
    Install {
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    /// Stop, restore changes, and unregister; retain protected binaries and audit files.
    Remove,
    Show,
    #[command(hide = true)]
    Run {
        #[arg(long)]
        config: PathBuf,
    },
}
fn main() -> ExitCode {
    let result = match Args::parse().command {
        Command::Install { directory } => directory
            .map_or_else(platform::admin::install_dir, Ok)
            .and_then(|p| platform::admin::install(&p)),
        Command::Remove => platform::admin::remove(),
        Command::Show => platform::admin::show(),
        Command::Run { config } => platform::admin::run(&config).map(|_| String::new()),
    };
    match result {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn binary_name_matches_names_table() {
        assert_eq!(env!("CARGO_BIN_NAME"), proto::names::ADMIN_BIN);
    }
}
