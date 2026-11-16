//! `bloch-tui`: the fleet-pattern terminal console for bloch
//! (programme D-1562, pattern DESIGN §D2).
//!
//! bloch is the fleet's lesson sink: ranked, budgeted codebase maps for
//! coding agents, session-handoff fidelity, and the lesson / outcome
//! record system whose law is that lessons carry `origin_lane` +
//! `content_label` + `evidence` from birth (bloch clause, D-1030).
//! This console makes that law visible and enforceable at the point of
//! review: the Lesson Candidates inbox is the centrepiece, and a
//! candidate that would violate the lane law for this host is shown
//! quarantined and cannot be accepted from the TUI.
//!
//! # Layout (DESIGN §D2)
//!
//! - [`facade`]: the narrow [`facade::BlochFacade`] trait the UI
//!   programs against, with an in-memory fake for tests and the real
//!   library-backed facade.
//! - [`candidate`]: the lesson-candidate type and the lane law.
//! - [`triggers`]: the DESIGN §D4 rule shape (data, parsed bounded).
//! - [`app`]: the event loop + screen state machine, plan/auto modes,
//!   and the non-TTY line mode sharing the same execute path.
//! - [`panels`]: pure ratatui widget constructors with named
//!   truncation bounds, tested through `TestBackend`.
//! - [`screens`]: the member's screens as plain text lines, shared by
//!   the full-screen draw and the line mode.
//!
//! # Limits
//!
//! Every queue, buffer, input line, and payload carries a named bound:
//! [`LOG_LINES_MAX`], [`INPUT_LINE_BYTES_MAX`], [`CANDIDATES_MAX`],
//! [`PAYLOAD_BYTES_MAX`], [`BUDGET_CHARS_MIN`], [`BUDGET_CHARS_MAX`].

#![forbid(unsafe_code)]

use std::fmt;

pub mod app;
pub mod candidate;
pub mod facade;
pub mod panels;
pub mod screens;
pub mod triggers;

pub use app::{App, LineAction, Mode, Screen, StagedAction, StagedKind};
pub use candidate::{CandidateState, LessonCandidate};
pub use facade::{BlochFacade, FakeFacade, LibFacade, MapSummary, RunEvent, StatusInfo};
pub use triggers::TriggerRule;

/// Maximum lines retained in the Log / Live buffer (oldest dropped).
pub const LOG_LINES_MAX: usize = 500;
/// Maximum bytes in one line-mode input line, newline included.
pub const INPUT_LINE_BYTES_MAX: usize = 8_192;
/// Maximum lesson candidates held in the inbox view at once.
pub const CANDIDATES_MAX: usize = 200;
/// Maximum bytes in one candidate payload (mirrors the server's
/// `PROPOSE_CONTENT_MAX` of 64,000 characters, counted in bytes here
/// so the bound holds before any decoding work).
pub const PAYLOAD_BYTES_MAX: usize = 64_000;
/// Maximum bytes read from one inbox candidate file.
pub const CANDIDATE_FILE_BYTES_MAX: u64 = 128_000;
/// Maximum characters in one candidate's evidence summary.
pub const EVIDENCE_CHARS_MAX: usize = 512;
/// Maximum characters in one candidate's rule id.
pub const RULE_ID_CHARS_MAX: usize = 128;
/// Maximum characters in one candidate id.
pub const CANDIDATE_ID_CHARS_MAX: usize = 256;
/// Smallest accepted map budget, in rendered characters.
pub const BUDGET_CHARS_MIN: usize = 500;
/// Largest accepted map budget, in rendered characters.
pub const BUDGET_CHARS_MAX: usize = 1_000_000;
/// Default map budget, in rendered characters (the CLI's default).
pub const BUDGET_CHARS_DEFAULT: usize = 12_000;
/// Maximum characters in the rendered session-handoff view.
pub const HANDOFF_CHARS_MAX: usize = 24_000;
/// Maximum audit lines tailed into the Fleet panel.
pub const AUDIT_TAIL_MAX: usize = 50;

/// Typed failures for the TUI's own operations. Expected failures
/// (bad input, lane-law refusals, missing entries) are values, never
/// panics; internal invariants use `assert!` per Tiger Style.
#[derive(Debug)]
pub enum TuiError {
    /// Caller-supplied input failed validation.
    Invalid(String),
    /// The lane law refuses this candidate for this host.
    Quarantined(String),
    /// A named entry (candidate id, file) does not exist.
    NotFound(String),
    /// The member library or a store reported a failure.
    Backend(String),
    /// An I/O failure at the terminal or filesystem boundary.
    Io(std::io::Error),
}

impl fmt::Display for TuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TuiError::Invalid(msg) => write!(f, "invalid: {msg}"),
            TuiError::Quarantined(msg) => write!(f, "quarantined: {msg}"),
            TuiError::NotFound(msg) => write!(f, "not_found: {msg}"),
            TuiError::Backend(msg) => write!(f, "backend: {msg}"),
            TuiError::Io(err) => write!(f, "io: {err}"),
        }
    }
}

impl std::error::Error for TuiError {}

impl From<std::io::Error> for TuiError {
    fn from(err: std::io::Error) -> TuiError {
        TuiError::Io(err)
    }
}

impl From<bloch::BlochError> for TuiError {
    fn from(err: bloch::BlochError) -> TuiError {
        TuiError::Backend(err.to_string())
    }
}

/// Validate a map budget against the named bounds. Budgets are the
/// product's core control, so out-of-range values are refused, never
/// clamped silently.
pub fn validate_budget(budget_chars: usize) -> Result<(), TuiError> {
    if budget_chars < BUDGET_CHARS_MIN {
        return Err(TuiError::Invalid(format!(
            "budget {budget_chars} chars is below BUDGET_CHARS_MIN {BUDGET_CHARS_MIN}"
        )));
    }
    if budget_chars > BUDGET_CHARS_MAX {
        return Err(TuiError::Invalid(format!(
            "budget {budget_chars} chars exceeds BUDGET_CHARS_MAX {BUDGET_CHARS_MAX}"
        )));
    }
    Ok(())
}
