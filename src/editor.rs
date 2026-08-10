//! Opening the user's `$EDITOR` on a scratch buffer, behind a trait so it
//! can be faked in tests.
//!
//! Production uses [`ProcessEditor`], which follows git's convention: the
//! editor command is `$VISUAL`, else `$EDITOR`, else `vim`, and it is run
//! through the shell so the variable may carry arguments (`code --wait`,
//! `emacsclient -nw`). The buffer is a real file in a temp directory, so
//! editors that save by rename — vim's default `backupcopy=auto` does —
//! leave the result where we can read it.
//!
//! Exiting the editor non-zero means "abort": [`Editor::edit`] returns
//! [`None`] and the caller leaves state alone. In vim that's `:cq`.

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// The editor used when neither `$VISUAL` nor `$EDITOR` is set.
pub const DEFAULT_EDITOR: &str = "vim";

/// Abstraction over "let the user edit this text".
pub trait Editor {
    /// Open `initial` for editing and return the saved text, or [`None`]
    /// if the user aborted.
    ///
    /// `filename` is the name the buffer gets, purely so the editor can
    /// pick syntax highlighting and the user can see what they're editing.
    fn edit(&self, initial: &str, filename: &str) -> Result<Option<String>>;
}

/// Spawns the user's real editor.
pub struct ProcessEditor {
    program: String,
}

impl ProcessEditor {
    /// Resolve the editor command from the environment: `$VISUAL`, else
    /// `$EDITOR`, else [`DEFAULT_EDITOR`].
    pub fn from_env() -> Self {
        let program = std::env::var("VISUAL")
            .ok()
            .or_else(|| std::env::var("EDITOR").ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_EDITOR.to_string());
        Self { program }
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// The command that opens `path`.
    ///
    /// On unix this goes through `sh -c '<editor> "$@"' <editor> <path>`,
    /// the same shape git uses, so `$EDITOR` may hold arguments or shell
    /// syntax rather than just a program name. Elsewhere we fall back to
    /// splitting on whitespace, which covers the common `prog --wait` case
    /// without inventing a shell.
    #[cfg(unix)]
    fn command(&self, path: &Path) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(format!("{} \"$@\"", self.program))
            .arg(&self.program)
            .arg(path);
        c
    }

    #[cfg(not(unix))]
    fn command(&self, path: &Path) -> Command {
        let mut parts = self.program.split_whitespace();
        let program = parts.next().unwrap_or(DEFAULT_EDITOR);
        let mut c = Command::new(program);
        c.args(parts).arg(path);
        c
    }
}

impl Editor for ProcessEditor {
    fn edit(&self, initial: &str, filename: &str) -> Result<Option<String>> {
        // A whole directory, not a bare temp file: vim writes `4913`
        // probe files and swap files beside the buffer, and may replace it
        // by rename. Giving it a directory of its own keeps that mess
        // contained and cleaned up, and lets the buffer carry the name we
        // want the user to see.
        let dir = tempfile::Builder::new()
            .prefix("choochoo-")
            .tempdir()
            .map_err(|e| Error::Io {
                path: std::env::temp_dir(),
                source: e,
            })?;
        let path = dir.path().join(filename);
        let mut seed = initial.to_string();
        if !seed.is_empty() && !seed.ends_with('\n') {
            seed.push('\n');
        }
        std::fs::write(&path, &seed).map_err(|e| Error::Io {
            path: path.clone(),
            source: e,
        })?;

        // Inherited stdio: the editor takes over this terminal.
        let status = self
            .command(&path)
            .status()
            .map_err(|source| Error::EditorLaunch {
                program: self.program.clone(),
                source,
            })?;
        if !status.success() {
            return Ok(None);
        }

        let edited = std::fs::read_to_string(&path).map_err(|e| Error::Io {
            path: path.clone(),
            source: e,
        })?;
        Ok(Some(edited))
    }
}

// ---------------------------------------------------------------------------
// Fake implementation (used by unit tests)
// ---------------------------------------------------------------------------

/// Test double: hands back a canned result and records what it was shown.
pub struct FakeEditor {
    reply: Option<String>,
    seen: std::sync::Mutex<Option<String>>,
}

impl FakeEditor {
    /// An editor whose user replaces the buffer with `text` and saves.
    pub fn saving(text: impl Into<String>) -> Self {
        Self {
            reply: Some(text.into()),
            seen: std::sync::Mutex::new(None),
        }
    }

    /// An editor whose user quits without saving.
    pub fn aborting() -> Self {
        Self {
            reply: None,
            seen: std::sync::Mutex::new(None),
        }
    }

    /// The buffer contents the editor was opened on, once it has been.
    pub fn seen(&self) -> Option<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Editor for FakeEditor {
    fn edit(&self, initial: &str, _filename: &str) -> Result<Option<String>> {
        *self.seen.lock().unwrap() = Some(initial.to_string());
        Ok(self.reply.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_records_the_buffer_and_returns_the_reply() {
        let ed = FakeEditor::saving("new text");
        assert_eq!(ed.edit("old text", "F.md").unwrap().as_deref(), Some("new text"));
        assert_eq!(ed.seen().as_deref(), Some("old text"));
    }

    #[test]
    fn aborting_fake_returns_none() {
        let ed = FakeEditor::aborting();
        assert_eq!(ed.edit("old", "F.md").unwrap(), None);
    }

    /// `$VISUAL` wins over `$EDITOR`, and an empty value doesn't count as
    /// a choice — that last part matters because `EDITOR=` in a shell
    /// profile is a common way to *unset* it.
    #[test]
    fn program_resolution_order() {
        // Not using `from_env` here: tests share a process environment, so
        // mutating it races other tests. Exercise the same precedence.
        let pick = |visual: Option<&str>, editor: Option<&str>| {
            visual
                .map(str::to_string)
                .or_else(|| editor.map(str::to_string))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_EDITOR.to_string())
        };
        assert_eq!(pick(Some("nvim"), Some("nano")), "nvim");
        assert_eq!(pick(None, Some("nano")), "nano");
        assert_eq!(pick(None, None), "vim");
        assert_eq!(pick(Some(""), None), "vim");
    }

    #[cfg(unix)]
    #[test]
    fn process_editor_round_trips_through_a_real_command() {
        // `sed -i` stands in for a human: it edits the buffer in place and
        // exits zero, so this exercises write -> spawn -> read back.
        let ed = ProcessEditor {
            program: "sed -i.bak s/before/after/".to_string(),
        };
        let out = ed.edit("before\n", "PR_TRAIN_CONTEXT.md").unwrap();
        assert_eq!(out.as_deref(), Some("after\n"));
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_is_an_abort() {
        let ed = ProcessEditor {
            program: "false".to_string(),
        };
        assert_eq!(ed.edit("anything", "F.md").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_editor_exits_nonzero_and_so_aborts() {
        // `sh -c` reports "command not found" by exiting 127 (having said
        // so on stderr) rather than failing to spawn, so this lands as an
        // abort. `Error::EditorLaunch` covers `sh` itself being missing.
        let ed = ProcessEditor {
            program: "choochoo-no-such-editor".to_string(),
        };
        assert_eq!(ed.edit("x", "F.md").unwrap(), None);
    }
}
