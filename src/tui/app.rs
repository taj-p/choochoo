//! TUI application state machine.
//!
//! Kept deliberately decoupled from terminal IO and ratatui types so that
//! navigation, selection, and side-effect requests can be unit-tested by
//! feeding [`KeyAction`]s in and asserting on the resulting [`Effect`]s
//! and view-state.

use crate::error::Result;
use crate::state::{StateFile, Store};

/// Logical key actions, normalized from raw crossterm events. The
/// translation lives in [`super::keymap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Up,
    Down,
    Enter,
    Back,
    Quit,
    Checkout,
    Rebase,
    Push,
    OpenPr,
    MoveUp,
    MoveDown,
}

/// What the controller should do after handling a key. The TUI driver
/// turns these into actual side-effects (calling into `train::*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    Checkout { train: String, branch: String },
    Rebase { train: String },
    Push { train: String },
    OpenPr { train: String },
    Reorder {
        train: String,
        branch: String,
        position: crate::train::reorder::Position,
        relative_to: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    TrainsList { selected: usize },
    TrainDetail { train: String, selected: usize },
}

#[derive(Debug, Clone)]
pub struct App {
    pub state: StateFile,
    pub view: View,
    pub status: String,
}

impl App {
    pub fn new(state: StateFile) -> Self {
        Self {
            state,
            view: View::TrainsList { selected: 0 },
            status: String::new(),
        }
    }

    /// Reload state from the store (after side effects mutate it).
    pub fn reload(&mut self, store: &Store) -> Result<()> {
        self.state = store.load()?;
        // Re-validate the current selection.
        match &mut self.view {
            View::TrainsList { selected } => {
                let n = self.state.trains.len();
                if n == 0 {
                    *selected = 0;
                } else if *selected >= n {
                    *selected = n - 1;
                }
            }
            View::TrainDetail { train, selected } => {
                if let Some(t) = self.state.trains.get(train) {
                    let n = t.branches.len();
                    if n == 0 {
                        *selected = 0;
                    } else if *selected >= n {
                        *selected = n - 1;
                    }
                } else {
                    self.view = View::TrainsList { selected: 0 };
                }
            }
        }
        Ok(())
    }

    pub fn trains_sorted(&self) -> Vec<&str> {
        self.state.trains.keys().map(String::as_str).collect()
    }

    pub fn selected_train_name(&self) -> Option<&str> {
        match &self.view {
            View::TrainsList { selected } => self.trains_sorted().get(*selected).copied(),
            View::TrainDetail { train, .. } => Some(train.as_str()),
        }
    }

    pub fn selected_branch(&self) -> Option<&str> {
        if let View::TrainDetail { train, selected } = &self.view {
            self.state
                .trains
                .get(train)
                .and_then(|t| t.branches.get(*selected))
                .map(String::as_str)
        } else {
            None
        }
    }

    pub fn handle(&mut self, action: KeyAction) -> Effect {
        match action {
            KeyAction::Quit => Effect::Quit,
            KeyAction::Up => self.move_selection(-1),
            KeyAction::Down => self.move_selection(1),
            KeyAction::Enter => self.enter(),
            KeyAction::Back => self.back(),
            KeyAction::Checkout => self.effect_checkout(),
            KeyAction::Rebase => self
                .selected_train_name()
                .map(|t| Effect::Rebase { train: t.into() })
                .unwrap_or(Effect::None),
            KeyAction::Push => self
                .selected_train_name()
                .map(|t| Effect::Push { train: t.into() })
                .unwrap_or(Effect::None),
            KeyAction::OpenPr => self
                .selected_train_name()
                .map(|t| Effect::OpenPr { train: t.into() })
                .unwrap_or(Effect::None),
            KeyAction::MoveUp => self.effect_move(-1),
            KeyAction::MoveDown => self.effect_move(1),
        }
    }

    fn move_selection(&mut self, delta: isize) -> Effect {
        match &mut self.view {
            View::TrainsList { selected } => {
                let n = self.state.trains.len();
                if n == 0 {
                    return Effect::None;
                }
                *selected = clamp(*selected as isize + delta, n);
            }
            View::TrainDetail { train, selected } => {
                let n = self
                    .state
                    .trains
                    .get(train)
                    .map(|t| t.branches.len())
                    .unwrap_or(0);
                if n == 0 {
                    return Effect::None;
                }
                *selected = clamp(*selected as isize + delta, n);
            }
        }
        Effect::None
    }

    fn enter(&mut self) -> Effect {
        if let View::TrainsList { selected } = &self.view {
            let names = self.trains_sorted();
            if let Some(name) = names.get(*selected) {
                self.view = View::TrainDetail {
                    train: name.to_string(),
                    selected: 0,
                };
            }
        }
        Effect::None
    }

    fn back(&mut self) -> Effect {
        if let View::TrainDetail { train, .. } = &self.view {
            // Restore the selection of the train we were viewing.
            let names = self.trains_sorted();
            let idx = names.iter().position(|n| *n == train.as_str()).unwrap_or(0);
            self.view = View::TrainsList { selected: idx };
        }
        Effect::None
    }

    fn effect_checkout(&self) -> Effect {
        if let View::TrainDetail { train, .. } = &self.view {
            if let Some(branch) = self.selected_branch() {
                return Effect::Checkout {
                    train: train.clone(),
                    branch: branch.to_string(),
                };
            }
        }
        Effect::None
    }

    fn effect_move(&self, delta: isize) -> Effect {
        let View::TrainDetail { train, selected } = &self.view else {
            return Effect::None;
        };
        let Some(t) = self.state.trains.get(train) else {
            return Effect::None;
        };
        let i = *selected;
        let n = t.branches.len();
        if n < 2 {
            return Effect::None;
        }
        let target = (i as isize + delta).clamp(0, n as isize - 1) as usize;
        if target == i {
            return Effect::None;
        }
        let position = if delta < 0 {
            crate::train::reorder::Position::Before
        } else {
            crate::train::reorder::Position::After
        };
        Effect::Reorder {
            train: train.clone(),
            branch: t.branches[i].clone(),
            position,
            relative_to: t.branches[target].clone(),
        }
    }
}

fn clamp(v: isize, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let max = n as isize - 1;
    v.clamp(0, max) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Train;

    fn s_with_trains(specs: &[(&str, &[&str])]) -> StateFile {
        let mut s = StateFile::default();
        for (name, branches) in specs {
            let mut t = Train::new(*name, "main");
            t.branches = branches.iter().map(|b| (*b).to_string()).collect();
            s.trains.insert((*name).to_string(), t);
        }
        if let Some((name, _)) = specs.first() {
            s.active = Some((*name).to_string());
        }
        s
    }

    #[test]
    fn trains_list_navigation_clamps() {
        let s = s_with_trains(&[("a", &[]), ("b", &[]), ("c", &[])]);
        let mut app = App::new(s);
        app.handle(KeyAction::Up); // already at top
        assert_eq!(app.selected_train_name(), Some("a"));
        app.handle(KeyAction::Down);
        assert_eq!(app.selected_train_name(), Some("b"));
        app.handle(KeyAction::Down);
        app.handle(KeyAction::Down); // clamps
        assert_eq!(app.selected_train_name(), Some("c"));
    }

    #[test]
    fn enter_drills_into_train() {
        let s = s_with_trains(&[("a", &["x", "y"])]);
        let mut app = App::new(s);
        app.handle(KeyAction::Enter);
        assert!(matches!(app.view, View::TrainDetail { ref train, .. } if train == "a"));
        assert_eq!(app.selected_branch(), Some("x"));
    }

    #[test]
    fn back_returns_to_list_selecting_same_train() {
        let s = s_with_trains(&[("a", &[]), ("b", &["x"]), ("c", &[])]);
        let mut app = App::new(s);
        app.handle(KeyAction::Down); // b
        app.handle(KeyAction::Enter);
        app.handle(KeyAction::Back);
        assert!(matches!(app.view, View::TrainsList { selected: 1 }));
    }

    #[test]
    fn checkout_returns_effect_with_selected_branch() {
        let s = s_with_trains(&[("a", &["x", "y"])]);
        let mut app = App::new(s);
        app.handle(KeyAction::Enter);
        app.handle(KeyAction::Down);
        let eff = app.handle(KeyAction::Checkout);
        assert_eq!(
            eff,
            Effect::Checkout {
                train: "a".into(),
                branch: "y".into()
            }
        );
    }

    #[test]
    fn rebase_push_pr_emit_effects_with_train_name() {
        let s = s_with_trains(&[("a", &["x"])]);
        let mut app = App::new(s);
        assert_eq!(
            app.handle(KeyAction::Rebase),
            Effect::Rebase { train: "a".into() }
        );
        assert_eq!(
            app.handle(KeyAction::Push),
            Effect::Push { train: "a".into() }
        );
        assert_eq!(
            app.handle(KeyAction::OpenPr),
            Effect::OpenPr { train: "a".into() }
        );
    }

    #[test]
    fn quit_emits_quit_effect() {
        let s = s_with_trains(&[("a", &[])]);
        let mut app = App::new(s);
        assert_eq!(app.handle(KeyAction::Quit), Effect::Quit);
    }

    #[test]
    fn move_up_in_train_detail_emits_reorder_before_predecessor() {
        let s = s_with_trains(&[("a", &["x", "y", "z"])]);
        let mut app = App::new(s);
        app.handle(KeyAction::Enter);
        app.handle(KeyAction::Down); // selects y
        let eff = app.handle(KeyAction::MoveUp);
        assert_eq!(
            eff,
            Effect::Reorder {
                train: "a".into(),
                branch: "y".into(),
                position: crate::train::reorder::Position::Before,
                relative_to: "x".into(),
            }
        );
    }

    #[test]
    fn move_down_at_bottom_is_noop() {
        let s = s_with_trains(&[("a", &["x", "y"])]);
        let mut app = App::new(s);
        app.handle(KeyAction::Enter);
        app.handle(KeyAction::Down); // selects y (last)
        assert_eq!(app.handle(KeyAction::MoveDown), Effect::None);
    }

    #[test]
    fn navigation_in_list_with_no_trains_is_noop() {
        let mut app = App::new(StateFile::default());
        assert_eq!(app.handle(KeyAction::Up), Effect::None);
        assert_eq!(app.handle(KeyAction::Down), Effect::None);
        assert_eq!(app.handle(KeyAction::Enter), Effect::None);
    }
}
