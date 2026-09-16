//! The single benshi binary.
//!
//! One binary, one process. Invoked with arguments it is a client speaking to a
//! running daemon over a local socket. Invoked with none it will run the daemon,
//! which is not wired yet and says so.
//!
//! The daemon must run inside the user's interactive session on every platform:
//! from Windows 11, the UWP APIs that SMTC detection depends on are unavailable
//! to a non-interactive session, which rules out running as a system service.

use std::process::ExitCode;

use benshi_cli::Cli;
use clap::Parser;

fn main() -> ExitCode {
    // Arguments and not a subcommand name, because the daemon is what benshi
    // does when asked for nothing. `Cli::parse` would print a usage message
    // here and exit, which is the wrong answer to `benshi` on its own.
    if std::env::args_os().nth(1).is_none() {
        eprintln!("benshi: the daemon is not implemented yet");
        return ExitCode::FAILURE;
    }

    benshi_cli::main(Cli::parse())
}
