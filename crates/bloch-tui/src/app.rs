//! The screen state machine, the plan/auto staging discipline,
//! and the two run modes (full-screen ratatui + non-TTY line mode)
//! sharing one execute path (DESIGN §D2).
//!
//! Plan mode is the safety centre: staging a workflow or a
//! candidate decision records the exact action and makes **no**
//! facade call. Only [`App::approve`] executes a staged action;
//! [`App::abort`] discards it. Tests prove both halves against the
//! fake's call log.

use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

use ratatui::backend::CrosstermBackend;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::candidate::CandidateState;
use crate::facade::{BlochFacade, MapSummary, StatusInfo};
use crate::{
    validate_budget, LessonCandidate, TuiError, BUDGET_CHARS_DEFAULT, INPUT_LINE_BYTES_MAX,
    LOG_LINES_MAX,
};

/// The comparable-capability screens (DESIGN §D2). bloch has no
/// media use cases, so there is no Media screen — absent, not
/// stubbed (DESIGN §D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Member name/version, store status, gate state.
    Overview,
    /// Map builds with budgets + the session-handoff view.
    Workflows,
    /// The lesson-candidate inbox (the centrepiece).
    Inbox,
    /// Bounded, scrolling run-event view.
    Log,
    /// Trigger rules (empty by design) + inbound audit tail.
    Fleet,
}

impl Screen {
    /// All screens in tab order.
    pub fn all() -> [Screen; 5] {
        [
            Screen::Overview,
            Screen::Workflows,
            Screen::Inbox,
            Screen::Log,
            Screen::Fleet,
        ]
    }

    /// Display name.
    pub fn name(&self) -> &'static str {
        match self {
            Screen::Overview => "Overview",
            Screen::Workflows => "Workflows",
            Screen::Inbox => "Lesson Candidates",
            Screen::Log => "Log / Live",
            Screen::Fleet => "Fleet",
        }
    }

    /// Short slug used by the line mode (`screen inbox`).
    pub fn slug(&self) -> &'static str {
        match self {
            Screen::Overview => "overview",
            Screen::Workflows => "workflows",
            Screen::Inbox => "inbox",
            Screen::Log => "log",
            Screen::Fleet => "fleet",
        }
    }

    /// Parse a screen name or slug (case-insensitive, prefixes
    /// of either allowed).
    pub fn parse(text: &str) -> Option<Screen> {
        let t = text.trim().to_ascii_lowercase();
        if t.is_empty() {
            return None;
        }
        Screen::all()
            .into_iter()
            .find(|s| s.slug().starts_with(&t) || s.name().to_ascii_lowercase().starts_with(&t))
    }
}

/// Execution mode for workflow / decision actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Stage the action, show it, wait for approval.
    Plan,
    /// Execute declared operations directly.
    Auto,
}

/// What a staged action will do when approved.
#[derive(Debug, Clone, PartialEq)]
pub enum StagedKind {
    /// Build a map of `target` under `budget_chars`.
    BuildMap {
        /// Target directory.
        target: PathBuf,
        /// Rendered-character budget.
        budget_chars: usize,
    },
    /// Accept candidate `id` into the lesson store.
    AcceptCandidate {
        /// Candidate id.
        id: String,
    },
    /// Reject candidate `id` with `reason`.
    RejectCandidate {
        /// Candidate id.
        id: String,
        /// Recorded rejection reason.
        reason: String,
    },
}

/// One staged action: the exact call approval would make.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedAction {
    /// Human-readable label for the staging panel.
    pub label: String,
    /// The operation that approval executes.
    pub kind: StagedKind,
}

/// The console state machine over one facade.
pub struct App<F: BlochFacade> {
    /// The facade the UI programs against.
    pub facade: F,
    /// Current screen.
    pub screen: Screen,
    /// Plan / auto execution mode.
    pub mode: Mode,
    /// Current target repo (map / handoff / inbox root).
    pub target: PathBuf,
    /// Current map budget, in rendered characters.
    pub budget_chars: usize,
    /// Whether map builds ask bloch for the SCIP index.
    pub use_scip: bool,
    /// Bounded log buffer (oldest dropped past [`LOG_LINES_MAX`]).
    pub log: Vec<String>,
    /// The staged action awaiting approval, if any.
    pub staged: Option<StagedAction>,
    /// Cached inbox candidates (refreshed on demand).
    pub candidates: Vec<LessonCandidate>,
    /// Selected candidate index in the inbox.
    pub selected: usize,
    /// Last completed map summary.
    pub last_map: Option<MapSummary>,
    /// Last handoff text.
    pub handoff_text: String,
    /// Last status fetched.
    pub status: Option<StatusInfo>,
}

impl<F: BlochFacade> App<F> {
    /// A console in plan mode (the safe default) on `target`.
    pub fn new(facade: F, target: PathBuf) -> App<F> {
        App {
            facade,
            screen: Screen::Overview,
            mode: Mode::Plan,
            target,
            budget_chars: BUDGET_CHARS_DEFAULT,
            use_scip: false,
            log: Vec::new(),
            staged: None,
            candidates: Vec::new(),
            selected: 0,
            last_map: None,
            handoff_text: String::new(),
            status: None,
        }
    }

    /// Append one line to the bounded log.
    pub fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_LINES_MAX {
            self.log.remove(0);
        }
        self.log.push(line);
    }

    /// Refresh status + candidates from the facade.
    pub fn refresh(&mut self) -> Result<(), TuiError> {
        self.status = Some(self.facade.status()?);
        self.candidates = self.facade.list_candidates()?;
        if self.selected >= self.candidates.len() {
            self.selected = self.candidates.len().saturating_sub(1);
        }
        Ok(())
    }

    /// The host lane from the last status (general when unknown:
    /// fail closed, the wall is stricter on the general lane).
    pub fn host_lane(&self) -> bloch::Lane {
        self.status
            .as_ref()
            .map(|s| s.host_lane)
            .unwrap_or(bloch::Lane::General)
    }

    /// Stage (plan) or execute (auto) a map build.
    pub fn stage_map(&mut self) -> Result<String, TuiError> {
        validate_budget(self.budget_chars)?;
        let action = StagedAction {
            label: format!(
                "build map of {} (budget {} chars)",
                self.target.display(),
                self.budget_chars
            ),
            kind: StagedKind::BuildMap {
                target: self.target.clone(),
                budget_chars: self.budget_chars,
            },
        };
        self.dispatch(action)
    }

    /// Stage (plan) or execute (auto) accepting candidate `id`.
    ///
    /// The lane law is checked *here*, before staging: a
    /// quarantined or malformed candidate cannot even be staged,
    /// so no approval path exists for it in the TUI.
    pub fn stage_accept(&mut self, id: &str) -> Result<String, TuiError> {
        let candidates = self.facade.list_candidates()?;
        self.candidates = candidates;
        let candidate = self
            .candidates
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| TuiError::NotFound(format!("candidate '{id}'")))?;
        match candidate.disposition(self.host_lane()) {
            (CandidateState::Pending, _) => {}
            (CandidateState::Quarantined, reason) => {
                let reason = reason.unwrap_or_default();
                self.push_log(format!("refused accept {id}: {reason}"));
                return Err(TuiError::Quarantined(reason));
            }
            (CandidateState::Malformed, reason) => {
                let reason = reason.unwrap_or_default();
                self.push_log(format!("refused accept {id}: {reason}"));
                return Err(TuiError::Invalid(reason));
            }
        }
        let action = StagedAction {
            label: format!("accept candidate {id} into lessons.md"),
            kind: StagedKind::AcceptCandidate { id: id.to_string() },
        };
        self.dispatch(action)
    }

    /// Stage (plan) or execute (auto) rejecting candidate `id`.
    /// Rejection is lawful for every candidate, quarantined ones
    /// included — it moves nothing into the store.
    pub fn stage_reject(&mut self, id: &str, reason: &str) -> Result<String, TuiError> {
        let action = StagedAction {
            label: format!("reject candidate {id}: {reason}"),
            kind: StagedKind::RejectCandidate {
                id: id.to_string(),
                reason: reason.to_string(),
            },
        };
        self.dispatch(action)
    }

    /// Plan mode stages without any facade call; auto mode runs
    /// the action immediately through the same execute path.
    fn dispatch(&mut self, action: StagedAction) -> Result<String, TuiError> {
        match self.mode {
            Mode::Plan => {
                let label = action.label.clone();
                self.staged = Some(action);
                Ok(format!("STAGED: {label} (approve / abort)"))
            }
            Mode::Auto => {
                self.staged = Some(action);
                self.approve()
            }
        }
    }

    /// Execute the staged action. This is the only path from a
    /// staged state to a facade call.
    pub fn approve(&mut self) -> Result<String, TuiError> {
        let action = self
            .staged
            .take()
            .ok_or_else(|| TuiError::Invalid("nothing staged to approve".to_string()))?;
        let result = self.execute_staged(&action);
        match &result {
            Ok(_) => self.push_log(format!("done: {}", action.label)),
            Err(err) => self.push_log(format!("failed: {}: {err}", action.label)),
        }
        result
    }

    /// Discard the staged action without any facade call.
    pub fn abort(&mut self) -> String {
        match self.staged.take() {
            Some(action) => format!("ABORTED: {}", action.label),
            None => "nothing staged".to_string(),
        }
    }

    fn execute_staged(&mut self, action: &StagedAction) -> Result<String, TuiError> {
        match &action.kind {
            StagedKind::BuildMap {
                target,
                budget_chars,
            } => {
                let (summary, events) =
                    self.facade
                        .build_map(target, *budget_chars, self.use_scip)?;
                for event in &events {
                    self.push_log(event.line());
                }
                let text = format!(
                    "map: {} files, {} definitions, {} chars (budget {}, {})",
                    summary.files,
                    summary.definitions,
                    summary.total_chars,
                    summary.budget_chars,
                    if summary.truncated {
                        "truncated to budget"
                    } else {
                        "within budget"
                    }
                );
                self.last_map = Some(summary);
                Ok(text)
            }
            StagedKind::AcceptCandidate { id } => {
                let message = self.facade.accept_candidate(id)?;
                let _ = self.refresh();
                Ok(message)
            }
            StagedKind::RejectCandidate { id, reason } => {
                let message = self.facade.reject_candidate(id, reason)?;
                let _ = self.refresh();
                Ok(message)
            }
        }
    }

    /// Run the handoff workflow (a read; runs immediately in both
    /// modes, its events stream into the log).
    pub fn run_handoff(&mut self) -> Result<String, TuiError> {
        let text = self.facade.handoff(&self.target.clone())?;
        self.push_log(format!(
            "handoff: {} chars for {}",
            text.chars().count(),
            self.target.display()
        ));
        self.handoff_text = text.clone();
        Ok(text)
    }

    /// The shared execute path for one command line, used by the
    /// line mode and by tests. Returns the text to show.
    pub fn execute(&mut self, line: &str) -> String {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some((cmd, args)) = parts.split_first() else {
            return String::new();
        };
        let result: Result<String, TuiError> = match *cmd {
            "help" => Ok(HELP_TEXT.to_string()),
            "status" | "overview" => match self.refresh() {
                Ok(()) => Ok(crate::screens::overview_lines(self).join("\n")),
                Err(err) => Err(err),
            },
            "tools" => Ok(self.facade.tools().join("\n")),
            "screen" => {
                let name = args.first().copied().unwrap_or("");
                match Screen::parse(name) {
                    Some(screen) => {
                        self.screen = screen;
                        Ok(crate::screens::current_lines(self).join("\n"))
                    }
                    None => Err(TuiError::Invalid(format!("unknown screen '{name}'"))),
                }
            }
            "map" => {
                if let Some(path) = args.first() {
                    self.target = PathBuf::from(path);
                }
                if let Some(budget) = args.get(1) {
                    match budget.parse::<usize>() {
                        Ok(value) => self.budget_chars = value,
                        Err(_) => {
                            return format!("error: budget '{budget}' is not a number");
                        }
                    }
                }
                self.stage_map()
            }
            "handoff" => {
                if let Some(path) = args.first() {
                    self.target = PathBuf::from(path);
                }
                self.run_handoff()
            }
            "inbox" => match self.refresh() {
                Ok(()) => Ok(crate::screens::inbox_lines(self).join("\n")),
                Err(err) => Err(err),
            },
            "accept" => {
                let Some(id) = args.first() else {
                    return "error: accept needs a candidate id".to_string();
                };
                self.stage_accept(id)
            }
            "reject" => {
                let Some(id) = args.first() else {
                    return "error: reject needs a candidate id".to_string();
                };
                let reason = if args.len() > 1 {
                    args[1..].join(" ")
                } else {
                    "rejected from bloch-tui".to_string()
                };
                self.stage_reject(id, &reason)
            }
            "fleet" => Ok(crate::screens::fleet_lines(self).join("\n")),
            "log" => Ok(self.log.join("\n")),
            "plan" => {
                self.mode = Mode::Plan;
                Ok("mode: plan (actions stage for approval)".to_string())
            }
            "auto" => {
                self.mode = Mode::Auto;
                Ok("mode: auto (declared actions execute directly)".to_string())
            }
            "approve" => self.approve(),
            "abort" => Ok(self.abort()),
            other => Err(TuiError::Invalid(format!("unknown command '{other}'"))),
        };
        match result {
            Ok(text) => text,
            Err(err) => format!("error: {err}"),
        }
    }
}

/// Line-mode help text.
pub const HELP_TEXT: &str = "commands: status | tools | screen <name> | map [path] [budget] | \
     handoff [path] | inbox | accept <id> | reject <id> [reason] | fleet | log | \
     plan | auto | approve | abort | help | quit";

/// What the loops should do with one input line.
#[derive(Debug, PartialEq, Eq)]
pub enum LineAction {
    /// End the session (`quit` / `exit`).
    Quit,
    /// Ignore the line (empty after trimming).
    Skip,
    /// Execute it as a command; carries the trimmed text.
    Execute(String),
}

/// Classify one input line. Pure and unit-testable.
pub fn classify_line(line: &str) -> LineAction {
    let text = line.trim();
    if text.is_empty() {
        LineAction::Skip
    } else if text == "quit" || text == "exit" || text == "/quit" {
        LineAction::Quit
    } else {
        LineAction::Execute(text.to_string())
    }
}

/// Read one input line after printing `prompt`. Returns `None` on
/// EOF. The bound counts every byte read including the trailing
/// newline; an overlong line is an error, never a silent grow.
pub fn read_line(prompt: &str) -> io::Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut limited = io::stdin().take((INPUT_LINE_BYTES_MAX + 1) as u64);
    let mut line = String::new();
    let bytes = io::BufReader::new(&mut limited).read_line(&mut line)?;
    if bytes == 0 {
        return Ok(None);
    }
    check_input_bound(bytes)?;
    if line.ends_with('\n') {
        line.pop();
    }
    Ok(Some(line))
}

/// Reject a read whose total byte count exceeds the bound.
fn check_input_bound(bytes_read: usize) -> io::Result<()> {
    if bytes_read > INPUT_LINE_BYTES_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input line exceeds INPUT_LINE_BYTES_MAX bytes",
        ));
    }
    Ok(())
}

/// Run the non-TTY line mode: plain banner, the same execute path
/// as the full-screen mode, one result per command.
pub fn run_line_mode<F: BlochFacade>(app: &mut App<F>) -> io::Result<()> {
    println!("bloch-tui · lesson sink console (line mode)");
    println!("target: {}", app.target.display());
    println!("{HELP_TEXT}");
    while let Some(line) = read_line("bloch > ")? {
        match classify_line(&line) {
            LineAction::Quit => break,
            LineAction::Skip => continue,
            LineAction::Execute(text) => {
                let out = app.execute(&text);
                if !out.is_empty() {
                    println!("{out}");
                }
            }
        }
    }
    Ok(())
}

/// Run the full-screen ratatui mode. Keys: `1`–`5` / Tab switch
/// screens, `p` toggles plan/auto, Enter runs the screen's primary
/// action (map build on Workflows, accept selected on Inbox),
/// `r` rejects the selected candidate, `a` approves a staged
/// action, `x` aborts it, `[` / `]` halve / double the budget,
/// `j` / `k` move the inbox selection, `q` quits.
pub fn run_fullscreen<F: BlochFacade>(app: &mut App<F>) -> io::Result<()> {
    use crossterm::event::{self};
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use crossterm::ExecutableCommand as _;

    let _ = app.refresh();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let outcome = fullscreen_loop(
        app,
        &mut terminal,
        &mut |app, terminal| {
            let lines = crate::screens::current_lines(app);
            let title = format!(
                " bloch-tui · {} · mode {} ",
                app.screen.name(),
                if app.mode == Mode::Plan {
                    "plan"
                } else {
                    "auto"
                }
            );
            terminal.draw(|frame| {
                let panel = Paragraph::new(lines.join("\n"))
                    .block(ratatui::widgets::Block::bordered().title(title));
                frame.render_widget(panel, frame.area());
            })?;
            Ok(())
        },
        &event::read,
    );
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    disable_raw_mode()?;
    outcome
}

/// The full-screen draw callback (renders the current screen).
type DrawFn<'a, F> =
    dyn FnMut(&mut App<F>, &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> + 'a;

/// The terminal event source.
type EventFn = dyn Fn() -> io::Result<crossterm::event::Event>;

fn fullscreen_loop<F: BlochFacade>(
    app: &mut App<F>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    draw: &mut DrawFn<'_, F>,
    read_event: &EventFn,
) -> io::Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    loop {
        draw(app, terminal)?;
        let Event::Key(key) = read_event()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') => return Ok(()),
            KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                return Ok(());
            }
            KeyCode::Tab => {
                let screens = Screen::all();
                let idx = screens.iter().position(|s| *s == app.screen).unwrap_or(0);
                app.screen = screens[(idx + 1) % screens.len()];
            }
            KeyCode::Char(d @ '1'..='5') => {
                let idx = (d as usize) - ('1' as usize);
                app.screen = Screen::all()[idx];
            }
            KeyCode::Char('p') => {
                app.mode = if app.mode == Mode::Plan {
                    Mode::Auto
                } else {
                    Mode::Plan
                };
            }
            KeyCode::Char('a') => {
                let _ = app.approve();
            }
            KeyCode::Char('x') => {
                app.abort();
            }
            KeyCode::Char('[') => {
                app.budget_chars = (app.budget_chars / 2).max(crate::BUDGET_CHARS_MIN);
            }
            KeyCode::Char(']') => {
                app.budget_chars = (app.budget_chars * 2).min(crate::BUDGET_CHARS_MAX);
            }
            KeyCode::Char('j') => {
                if !app.candidates.is_empty() {
                    app.selected = (app.selected + 1).min(app.candidates.len() - 1);
                }
            }
            KeyCode::Char('k') => {
                app.selected = app.selected.saturating_sub(1);
            }
            KeyCode::Char('r') if app.screen == Screen::Inbox => {
                if let Some(candidate) = app.candidates.get(app.selected).cloned() {
                    let _ = app.stage_reject(&candidate.id, "rejected from bloch-tui");
                }
            }
            KeyCode::Enter => match app.screen {
                Screen::Workflows => {
                    let _ = app.stage_map();
                }
                Screen::Inbox => {
                    if let Some(candidate) = app.candidates.get(app.selected).cloned() {
                        let _ = app.stage_accept(&candidate.id);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::LessonCandidate;
    use crate::facade::FakeFacade;
    use bloch::{ContentLabel, Lane};

    fn candidate(id: &str, origin: Lane, label: ContentLabel) -> LessonCandidate {
        LessonCandidate {
            id: id.to_string(),
            rule_id: "rule-1".to_string(),
            origin_lane: Some(origin),
            content_label: Some(label),
            evidence: "gate output 2026-11-16".to_string(),
            payload: "lesson body".to_string(),
            source_file: None,
        }
    }

    fn app_with(candidates: Vec<LessonCandidate>) -> App<FakeFacade> {
        let mut facade = FakeFacade::new();
        facade.candidates = candidates;
        let mut app = App::new(facade, PathBuf::from("/tmp/fake-repo"));
        app.refresh().unwrap();
        app
    }

    #[test]
    fn plan_mode_stages_map_without_any_facade_call() {
        let mut app = app_with(vec![]);
        let out = app.execute("map");
        assert!(out.starts_with("STAGED:"), "{out}");
        assert!(app.staged.is_some());
        assert!(
            app.facade.calls.is_empty(),
            "plan mode must not call the facade: {:?}",
            app.facade.calls
        );
    }

    #[test]
    fn approve_executes_the_staged_map_once() {
        let mut app = app_with(vec![]);
        app.execute("map");
        let out = app.approve().unwrap();
        assert!(out.contains("map:"), "{out}");
        assert_eq!(app.facade.calls, vec!["map:12000".to_string()]);
        assert!(app.staged.is_none());
        assert!(app.last_map.is_some());
    }

    #[test]
    fn abort_discards_staged_map_without_calling() {
        let mut app = app_with(vec![]);
        app.execute("map");
        let out = app.abort();
        assert!(out.starts_with("ABORTED:"));
        assert!(app.facade.calls.is_empty());
        // Approval after abort has nothing to execute.
        assert!(app.approve().is_err());
        assert!(app.facade.calls.is_empty());
    }

    #[test]
    fn auto_mode_executes_map_immediately() {
        let mut app = app_with(vec![]);
        app.mode = Mode::Auto;
        let out = app.execute("map");
        assert!(out.contains("map:"), "{out}");
        assert_eq!(app.facade.calls.len(), 1);
    }

    #[test]
    fn plan_mode_accept_stages_then_approves() {
        let mut app = app_with(vec![candidate("c1", Lane::General, ContentLabel::Sfw)]);
        let out = app.execute("accept c1");
        assert!(out.starts_with("STAGED:"), "{out}");
        assert!(app.facade.calls.is_empty());
        let done = app.approve().unwrap();
        assert!(done.contains("promoted c1"), "{done}");
        assert_eq!(app.facade.calls, vec!["accept:c1".to_string()]);
    }

    #[test]
    fn quarantined_candidate_cannot_be_staged_or_accepted() {
        // The centrepiece adversarial case: an nsfw-pure candidate
        // into the qompass/sfw store. The accept path refuses at
        // staging time; the facade is never invoked, in either mode.
        for mode in [Mode::Plan, Mode::Auto] {
            let mut app = app_with(vec![candidate(
                "bad-1",
                Lane::Restricted,
                ContentLabel::Nsfw,
            )]);
            app.mode = mode;
            let out = app.execute("accept bad-1");
            assert!(out.contains("lane_wall_refusal"), "mode {mode:?}: {out}");
            assert!(app.staged.is_none(), "mode {mode:?}: nothing may be staged");
            assert!(
                app.facade.calls.is_empty(),
                "mode {mode:?}: facade must not be called: {:?}",
                app.facade.calls
            );
            // And the candidate is still there, un-accepted.
            assert_eq!(app.facade.candidates.len(), 1);
        }
    }

    #[test]
    fn malformed_candidate_accept_is_refused() {
        let mut bad = candidate("m1", Lane::General, ContentLabel::Sfw);
        bad.evidence = String::new();
        let mut app = app_with(vec![bad]);
        app.mode = Mode::Auto;
        let out = app.execute("accept m1");
        assert!(out.contains("unlabelled_candidate"), "{out}");
        assert!(app.facade.calls.is_empty());
    }

    #[test]
    fn quarantined_candidate_can_still_be_rejected() {
        let mut app = app_with(vec![candidate(
            "bad-1",
            Lane::Restricted,
            ContentLabel::Nsfw,
        )]);
        app.mode = Mode::Auto;
        let out = app.execute("reject bad-1 not for this lane");
        assert!(out.contains("rejected bad-1"), "{out}");
        assert!(app.facade.candidates.is_empty());
    }

    #[test]
    fn accept_unknown_candidate_is_typed_error() {
        let mut app = app_with(vec![]);
        app.mode = Mode::Auto;
        let out = app.execute("accept nope");
        assert!(out.contains("not_found"), "{out}");
    }

    #[test]
    fn budget_bounds_are_enforced_before_any_call() {
        let mut app = app_with(vec![]);
        app.mode = Mode::Auto;
        let out = app.execute("map /tmp/fake-repo 10");
        assert!(out.contains("below BUDGET_CHARS_MIN"), "{out}");
        let out = app.execute("map /tmp/fake-repo 99999999");
        assert!(out.contains("exceeds BUDGET_CHARS_MAX"), "{out}");
        assert!(app.facade.calls.is_empty());
    }

    #[test]
    fn log_buffer_is_bounded() {
        let mut app = app_with(vec![]);
        for i in 0..(LOG_LINES_MAX + 25) {
            app.push_log(format!("line {i}"));
        }
        assert_eq!(app.log.len(), LOG_LINES_MAX);
        assert_eq!(app.log[0], "line 25");
    }

    #[test]
    fn classify_line_variants() {
        assert_eq!(classify_line("quit"), LineAction::Quit);
        assert_eq!(classify_line("  exit "), LineAction::Quit);
        assert_eq!(classify_line(""), LineAction::Skip);
        assert_eq!(
            classify_line("status"),
            LineAction::Execute("status".to_string())
        );
        assert_eq!(
            classify_line("quitter"),
            LineAction::Execute("quitter".to_string())
        );
    }

    #[test]
    fn input_bound_counts_bytes() {
        assert!(check_input_bound(INPUT_LINE_BYTES_MAX).is_ok());
        assert!(check_input_bound(INPUT_LINE_BYTES_MAX + 1).is_err());
    }

    #[test]
    fn screen_parse_prefixes() {
        assert_eq!(Screen::parse("inbox"), Some(Screen::Inbox));
        assert_eq!(Screen::parse("Lesson"), Some(Screen::Inbox));
        assert_eq!(Screen::parse("fleet"), Some(Screen::Fleet));
        assert_eq!(Screen::parse("nope"), None);
        assert_eq!(Screen::parse(""), None);
    }
}
