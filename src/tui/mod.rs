//! Interactive ratatui-based UI.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, List, ListItem, Paragraph};

use crate::error::Result;
use crate::git::{ProcessGitRunner, PushMode};
use crate::github;
use crate::report::Reporter;
use crate::state::Store;
use crate::train;

pub mod app;

use app::{App, Effect, KeyAction, View};

/// Tiny [`Reporter`] adapter that just remembers the latest progress
/// line. The TUI can't render mid-call (we're blocking the event loop
/// while git runs), so we surface the final state once the operation
/// returns. We still use a Reporter so domain code stays uniform.
#[derive(Default)]
struct LatestStatusReporter {
    pub latest: Option<String>,
}

impl Reporter for LatestStatusReporter {
    fn start(&mut self, msg: &str) {
        self.latest = Some(msg.to_string());
    }
    fn ok(&mut self, detail: &str) {
        if let Some(prev) = &self.latest {
            self.latest = Some(if detail.is_empty() {
                format!("{prev}... ok")
            } else {
                format!("{prev}... ok ({detail})")
            });
        }
    }
    fn fail(&mut self, detail: &str) {
        if let Some(prev) = &self.latest {
            self.latest = Some(format!("{prev}... FAILED: {detail}"));
        }
    }
    fn info(&mut self, msg: &str) {
        self.latest = Some(msg.to_string());
    }
}

/// Launch the TUI bound to a real terminal.
pub fn run(store: &Store) -> Result<()> {
    let state = store.load()?;
    let mut app = App::new(state);
    show_store_warnings(&mut app, store);

    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut app, store);
    ratatui::restore();
    // Anything left unshown goes to stderr now that the alternate screen is
    // gone — a warning the user never saw would defeat the point.
    for warning in store.take_warnings() {
        eprintln!("warning: {warning}");
    }
    result
}

/// Fold any sync degradation into the status line.
///
/// The TUI holds the alternate screen, so writing warnings to stderr while
/// it is up would scribble over the display. They have to come through the
/// UI's own channel.
fn show_store_warnings(app: &mut App, store: &Store) {
    for warning in store.take_warnings() {
        app.status = if app.status.is_empty() {
            format!("warning: {warning}")
        } else {
            format!("{} | warning: {warning}", app.status)
        };
    }
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    store: &Store,
) -> Result<()> {
    loop {
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(crate::error::Error::BareIo)?;

        let Some(action) = poll_action()? else {
            continue;
        };

        match app.handle(action) {
            Effect::Quit => return Ok(()),
            Effect::None => {}
            Effect::Checkout { branch, .. } => {
                let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
                match train::checkout::run(
                    store,
                    &git,
                    &mut LatestStatusReporter::default(),
                    None,
                    &branch,
                    "origin",
                ) {
                    Ok(()) => {
                        app.status = format!("checked out `{branch}`");
                    }
                    Err(e) => {
                        app.status = format!("checkout failed: {e}");
                    }
                }
            }
            Effect::Rebase { train: t } => {
                let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
                let mut rep = LatestStatusReporter::default();
                match train::rebase::run(store, &git, &mut rep, Some(&t)) {
                    Ok(s) => {
                        app.status = format!("rebased {} branch(es)", s.rebased.len());
                    }
                    Err(e) => {
                        app.status = match rep.latest {
                            Some(last) => format!("{last} | rebase failed: {e}"),
                            None => format!("rebase failed: {e}"),
                        };
                    }
                }
                app.reload(store)?;
                show_store_warnings(app, store);
            }
            Effect::Push { train: t } => {
                let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
                let mut rep = LatestStatusReporter::default();
                match train::push::run(
                    store,
                    &git,
                    &mut rep,
                    Some(&t),
                    PushMode::ForceWithLease,
                    "origin",
                ) {
                    Ok(s) => app.status = format!("pushed {} branch(es)", s.pushed.len()),
                    Err(e) => {
                        app.status = match rep.latest {
                            Some(last) => format!("{last} | push failed: {e}"),
                            None => format!("push failed: {e}"),
                        };
                    }
                }
                app.reload(store)?;
                show_store_warnings(app, store);
            }
            Effect::OpenPr { train: t } => {
                let gh = github::make_runner()?;
                let mut rep = LatestStatusReporter::default();
                match train::pr::run(store, gh.as_ref(), &mut rep, Some(&t), false) {
                    Ok(s) => {
                        app.status = format!(
                            "PRs: {} created, {} updated",
                            s.created.len(),
                            s.updated.len()
                        );
                    }
                    Err(e) => {
                        app.status = match rep.latest {
                            Some(last) => format!("{last} | pr failed: {e}"),
                            None => format!("pr failed: {e}"),
                        };
                    }
                }
                app.reload(store)?;
                show_store_warnings(app, store);
            }
            Effect::Reorder {
                train: t,
                branch,
                position,
                relative_to,
            } => {
                match train::reorder::run(
                    store,
                    Some(&t),
                    &branch,
                    position,
                    &relative_to,
                ) {
                    Ok(()) => app.status = format!("moved `{branch}`"),
                    Err(e) => app.status = format!("move failed: {e}"),
                }
                app.reload(store)?;
                show_store_warnings(app, store);
            }
        }
    }
}

fn poll_action() -> Result<Option<KeyAction>> {
    if !event::poll(Duration::from_millis(250))
        .map_err(|e| crate::error::Error::BareIo(io::Error::other(e)))?
    {
        return Ok(None);
    }
    let ev = event::read().map_err(|e| crate::error::Error::BareIo(io::Error::other(e)))?;
    Ok(translate(&ev))
}

fn translate(ev: &Event) -> Option<KeyAction> {
    let Event::Key(k) = ev else {
        return None;
    };
    if k.kind != KeyEventKind::Press {
        return None;
    }
    Some(match k.code {
        KeyCode::Char('q') => KeyAction::Quit,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => KeyAction::Quit,
        KeyCode::Up | KeyCode::Char('k') => KeyAction::Up,
        KeyCode::Down | KeyCode::Char('j') => KeyAction::Down,
        KeyCode::Enter | KeyCode::Char('l') => KeyAction::Enter,
        KeyCode::Esc | KeyCode::Char('h') => KeyAction::Back,
        KeyCode::Char('K') => KeyAction::MoveUp,
        KeyCode::Char('J') => KeyAction::MoveDown,
        KeyCode::Char('o') => KeyAction::Checkout,
        KeyCode::Char('R') => KeyAction::Rebase,
        KeyCode::Char('P') => KeyAction::Push,
        KeyCode::Char('O') => KeyAction::OpenPr,
        _ => return None,
    })
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let title = "choochoo";
    frame.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        layout[0],
    );

    match &app.view {
        View::TrainsList { selected } => {
            let names = app.trains_sorted();
            let items: Vec<ListItem> = names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let active = app.state.active.as_deref() == Some(*name);
                    let n = app.state.trains.get(*name).map(|t| t.branches.len()).unwrap_or(0);
                    let line = format!(
                        "{} {} ({n} branches)",
                        if active { "*" } else { " " },
                        name
                    );
                    let style = if i == *selected {
                        Style::default().reversed()
                    } else {
                        Style::default()
                    };
                    ListItem::new(line).style(style)
                })
                .collect();
            let list = List::new(items).block(Block::bordered().title(" trains "));
            frame.render_widget(list, layout[1]);
        }
        View::TrainDetail { train, selected } => {
            let Some(t) = app.state.trains.get(train) else {
                return;
            };
            let title = format!(" train: {} (base: {}) ", t.name, t.base);
            let mut items: Vec<ListItem> = t
                .branches
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    let pr = t
                        .prs
                        .get(b)
                        .map(|p| format!("#{}", p.number))
                        .unwrap_or_else(|| "—".into());
                    let line = format!("{:>2}. {b}   {pr}", i + 1);
                    let style = if i == *selected {
                        Style::default().reversed()
                    } else {
                        Style::default()
                    };
                    ListItem::new(line).style(style)
                })
                .collect();
            // The combined branch isn't part of the stack, so it's listed
            // after it and never selectable.
            if let Some(agg) = &t.aggregate {
                let pr = agg
                    .pr
                    .as_ref()
                    .map(|p| format!("draft #{}", p.number))
                    .unwrap_or_else(|| "—".into());
                items.push(
                    ListItem::new(format!(" Σ. {}   {pr}", agg.branch))
                        .style(Style::default().add_modifier(Modifier::DIM)),
                );
            }
            let list = List::new(items).block(Block::bordered().title(title));
            frame.render_widget(list, layout[1]);
        }
    }

    let help = match &app.view {
        View::TrainsList { .. } => "j/k move  enter open  q quit",
        View::TrainDetail { .. } => {
            "j/k move  J/K reorder  o checkout  R rebase  P push  O pr  esc back  q quit"
        }
    };
    let bottom = if app.status.is_empty() {
        help.to_string()
    } else {
        format!("{help}  |  {}", app.status)
    };
    frame.render_widget(Paragraph::new(bottom), layout[2]);
}
