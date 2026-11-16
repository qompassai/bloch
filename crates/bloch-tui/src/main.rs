//! `bloch-tui` binary: parse args, build the real facade, run.
//!
//! Full-screen ratatui where a TTY exists; the non-TTY line mode
//! (same execute path) when stdout is piped or `--line` is given.
//!
//! Usage: `bloch-tui [--root <dir>] [--line] [--auto]`
//! - `--root <dir>`: target repo (maps, handoff, lesson inbox).
//!   Defaults to the current directory.
//! - `--line`: force line mode even on a TTY.
//! - `--auto`: start in auto mode (default is plan: actions stage
//!   for approval).

#![forbid(unsafe_code)]

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use bloch_tui::app::{run_fullscreen, run_line_mode};
use bloch_tui::{App, LibFacade, Mode};

fn main() -> ExitCode {
    let mut root = PathBuf::from(".");
    let mut force_line = false;
    let mut auto = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => match args.next() {
                Some(value) => root = PathBuf::from(value),
                None => {
                    eprintln!("bloch-tui: --root needs a directory argument");
                    return ExitCode::FAILURE;
                }
            },
            "--line" => force_line = true,
            "--auto" => auto = true,
            "--help" | "-h" => {
                println!(
                    "bloch-tui [--root <dir>] [--line] [--auto]\n\
                     Full-screen console on a TTY; line mode otherwise."
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("bloch-tui: unknown argument '{other}' (see --help)");
                return ExitCode::FAILURE;
            }
        }
    }
    let facade = LibFacade::new(root.clone());
    let mut app = App::new(facade, root);
    if auto {
        app.mode = Mode::Auto;
    }
    let result = if force_line || !std::io::stdout().is_terminal() {
        run_line_mode(&mut app)
    } else {
        run_fullscreen(&mut app)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bloch-tui: {err}");
            ExitCode::FAILURE
        }
    }
}
