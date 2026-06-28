//! Cairn — a modern terminal file manager for every filesystem.
//!
//! This is the application entry point: it sets up logging and a panic hook, parses the (currently
//! minimal) command line, and will eventually launch the TUI event loop. Cairn is in early
//! development; see `docs/IMPLEMENTATION_PLAN.md` for status and `docs/LLD.md` for the architecture.

mod cli;
mod logging;
mod panic;

use std::process::ExitCode;

/// The application name, surfaced in the UI and `--version` output.
pub(crate) const APP_NAME: &str = "cairn";

/// The application version, taken from the crate metadata.
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    panic::install_hook();

    match cli::parse(std::env::args().skip(1)) {
        cli::Invocation::Run => run(),
        cli::Invocation::Version => {
            println!("{APP_NAME} {APP_VERSION}");
            ExitCode::SUCCESS
        }
        cli::Invocation::Help => {
            print!("{}", cli::help_text());
            ExitCode::SUCCESS
        }
        cli::Invocation::Unknown(arg) => {
            eprintln!("{APP_NAME}: unknown argument '{arg}'\n");
            print!("{}", cli::help_text());
            ExitCode::FAILURE
        }
    }
}

/// Run the application proper. For now this only initializes logging and reports that the TUI is not
/// yet wired up; the event loop arrives with the M1 milestone.
fn run() -> ExitCode {
    logging::init();
    tracing::info!(version = APP_VERSION, "cairn starting");
    println!(
        "{APP_NAME} {APP_VERSION} — early development. The interactive TUI is not wired up yet; \
         see docs/IMPLEMENTATION_PLAN.md."
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_name_is_set() {
        assert_eq!(APP_NAME, "cairn");
    }

    #[test]
    fn version_is_nonempty() {
        assert!(!APP_VERSION.is_empty());
    }
}
