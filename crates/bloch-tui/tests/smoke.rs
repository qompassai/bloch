//! Headless smoke: the line-mode execute path answers commands
//! from plain strings (the pipe-shaped context DESIGN §D2 asks
//! every TUI to be testable in), and the first screen renders
//! under `TestBackend` (covered in `panels` unit tests).

use std::path::PathBuf;

use bloch_tui::{App, FakeFacade};

#[test]
fn line_mode_answers_status_and_inbox() {
    let facade = FakeFacade::new();
    let mut app = App::new(facade, PathBuf::from("/tmp/fake-repo"));
    let status = app.execute("status");
    assert!(status.contains("bloch 1.0.0"), "{status}");
    assert!(status.contains("host lane: general"), "{status}");
    let inbox = app.execute("inbox");
    assert!(inbox.contains("inbox empty"), "{inbox}");
    let help = app.execute("help");
    assert!(help.contains("accept <id>"), "{help}");
}

#[test]
fn line_mode_unknown_command_is_typed_error_text() {
    let facade = FakeFacade::new();
    let mut app = App::new(facade, PathBuf::from("/tmp/fake-repo"));
    let out = app.execute("frobnicate");
    assert!(out.contains("unknown command"), "{out}");
}
