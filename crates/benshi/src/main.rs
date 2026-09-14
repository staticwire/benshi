//! The single benshi binary.
//!
//! One binary, one process. Invoked with no arguments it runs the daemon;
//! invoked with arguments it is a client speaking to a running daemon over a
//! local socket. `--no-tray` omits the icon.
//!
//! The daemon must run inside the user's interactive session on every platform:
//! from Windows 11, the UWP APIs that SMTC detection depends on are unavailable
//! to a non-interactive session, which rules out running as a system service.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        eprintln!("benshi: the daemon is not implemented yet");
    } else {
        eprintln!(
            "benshi: the client is not implemented yet: {}",
            args.join(" ")
        );
    }

    ExitCode::FAILURE
}
