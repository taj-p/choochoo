//! Progress reporting for long-running domain operations.
//!
//! Operations like [`crate::train::push::run`], [`crate::train::pr::run`],
//! and [`crate::train::rebase::run`] take `&mut dyn Reporter` so they can
//! emit step-by-step updates without coupling the domain to stderr or any
//! particular UI. The CLI uses [`StderrReporter`]; the TUI folds messages
//! into its status bar; tests use [`NullReporter`] or
//! [`RecordingReporter`].
//!
//! Progress goes to stderr so stdout remains a clean stream of summaries
//! safe to pipe into other tools.

use std::io::{self, Write};

/// Sink for human-readable progress lines.
///
/// Implementations should be cheap to call; a domain op may emit one
/// [`Reporter::start`] then a [`Reporter::ok`] / [`Reporter::fail`] per
/// step in a tight loop.
pub trait Reporter {
    /// Begin a logical step. The convention is `<verb>ing <subject>`,
    /// optionally annotated with `(i/n)` progress.
    fn start(&mut self, msg: &str);
    /// Annotate the most recent [`start`] as completed; `detail` is
    /// appended (e.g. `"#42"` for a PR number, or `"unchanged"` to signal
    /// a no-op).
    fn ok(&mut self, detail: &str);
    /// Annotate the most recent [`start`] as failed.
    fn fail(&mut self, detail: &str);
    /// A standalone informational line (e.g. summary headers).
    fn info(&mut self, msg: &str);
}

/// Convenience helpers built on top of [`Reporter`].
pub trait ReporterExt: Reporter {
    /// Emit a `<verb> X (i/n)... ok [detail]` pair atomically.
    fn step_ok(&mut self, msg: &str, detail: &str) {
        self.start(msg);
        self.ok(detail);
    }
}

impl<R: Reporter + ?Sized> ReporterExt for R {}

// ---------------------------------------------------------------------------
// Implementations
// ---------------------------------------------------------------------------

/// Discards all progress events. Use in unit tests where the events are
/// irrelevant to the assertion.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullReporter;

impl Reporter for NullReporter {
    fn start(&mut self, _: &str) {}
    fn ok(&mut self, _: &str) {}
    fn fail(&mut self, _: &str) {}
    fn info(&mut self, _: &str) {}
}

/// Writes one line per event to stderr. The "start... ok" pattern is
/// rendered as a single line:
///
/// ```text
/// pushing `feat/a` (1/3)... ok
/// ```
///
/// On a TTY we could prettify with carriage returns later; for now we
/// always emit newlines so output is readable in CI logs and pipes.
pub struct StderrReporter {
    pending_start: Option<String>,
}

impl StderrReporter {
    pub fn new() -> Self {
        Self {
            pending_start: None,
        }
    }

    fn flush_pending(&mut self, suffix: &str) {
        let Some(start) = self.pending_start.take() else {
            return;
        };
        let mut out = io::stderr().lock();
        let _ = writeln!(out, "{start}... {suffix}");
    }
}

impl Default for StderrReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter for StderrReporter {
    fn start(&mut self, msg: &str) {
        self.flush_pending("interrupted");
        self.pending_start = Some(msg.to_string());
    }

    fn ok(&mut self, detail: &str) {
        let suffix = if detail.is_empty() {
            "ok".to_string()
        } else {
            format!("ok ({detail})")
        };
        self.flush_pending(&suffix);
    }

    fn fail(&mut self, detail: &str) {
        let suffix = if detail.is_empty() {
            "FAILED".to_string()
        } else {
            format!("FAILED: {detail}")
        };
        self.flush_pending(&suffix);
    }

    fn info(&mut self, msg: &str) {
        // info lines pre-empt any pending start (rare; here for safety).
        self.flush_pending("interrupted");
        let mut out = io::stderr().lock();
        let _ = writeln!(out, "{msg}");
    }
}

impl Drop for StderrReporter {
    fn drop(&mut self) {
        // If the program panics or returns mid-step, we still want a sane
        // log entry to anchor the failure.
        self.flush_pending("interrupted");
    }
}

/// Records every event in a `Vec<String>` for assertion in tests.
#[derive(Debug, Default)]
pub struct RecordingReporter {
    pub events: Vec<String>,
    pending: Option<String>,
}

impl RecordingReporter {
    pub fn new() -> Self {
        Self::default()
    }
    /// Joined view of all events, one per line.
    pub fn joined(&self) -> String {
        self.events.join("\n")
    }
}

impl Reporter for RecordingReporter {
    fn start(&mut self, msg: &str) {
        if let Some(p) = self.pending.take() {
            self.events.push(format!("{p}... interrupted"));
        }
        self.pending = Some(msg.to_string());
    }
    fn ok(&mut self, detail: &str) {
        if let Some(p) = self.pending.take() {
            let suffix = if detail.is_empty() {
                "ok".to_string()
            } else {
                format!("ok ({detail})")
            };
            self.events.push(format!("{p}... {suffix}"));
        }
    }
    fn fail(&mut self, detail: &str) {
        if let Some(p) = self.pending.take() {
            self.events.push(format!("{p}... FAILED: {detail}"));
        }
    }
    fn info(&mut self, msg: &str) {
        self.events.push(msg.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_reporter_swallows_events() {
        let mut r = NullReporter;
        r.start("a");
        r.ok("");
        r.fail("");
        r.info("");
    }

    #[test]
    fn recording_reporter_pairs_start_and_ok() {
        let mut r = RecordingReporter::new();
        r.start("pushing `a` (1/2)");
        r.ok("");
        r.start("pushing `b` (2/2)");
        r.ok("12 commits");
        assert_eq!(
            r.events,
            vec![
                "pushing `a` (1/2)... ok".to_string(),
                "pushing `b` (2/2)... ok (12 commits)".to_string(),
            ]
        );
    }

    #[test]
    fn recording_reporter_handles_failures_and_info() {
        let mut r = RecordingReporter::new();
        r.info("== rebase ==");
        r.start("rebasing `a`");
        r.fail("conflict in foo.txt");
        assert_eq!(
            r.events,
            vec![
                "== rebase ==".to_string(),
                "rebasing `a`... FAILED: conflict in foo.txt".to_string(),
            ]
        );
    }

    #[test]
    fn step_ok_helper_is_atomic() {
        let mut r = RecordingReporter::new();
        r.step_ok("syncing `a`", "#42");
        assert_eq!(r.events, vec!["syncing `a`... ok (#42)"]);
    }

    #[test]
    fn unfinished_start_is_marked_interrupted_on_next_event() {
        let mut r = RecordingReporter::new();
        r.start("a");
        r.start("b"); // no ok/fail for `a`
        r.ok("");
        assert_eq!(
            r.events,
            vec![
                "a... interrupted".to_string(),
                "b... ok".to_string(),
            ]
        );
    }
}
