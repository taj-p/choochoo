//! `choo push` — push every branch in a train.

use std::path::Path;

use crate::error::Result;
use crate::git::{GitRunner, PushMode};
use crate::report::Reporter;
use crate::state;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSummary {
    pub train: String,
    pub pushed: Vec<String>,
    pub mode: PushMode,
}

pub fn run(
    repo_root: &Path,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
    mode: PushMode,
    remote: &str,
) -> Result<PushSummary> {
    let mut state = state::load(repo_root)?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    let branches = state.train(&train_name)?.branches.clone();
    let total = branches.len();
    let mode_label = match mode {
        PushMode::ForceWithLease => "force-with-lease",
        PushMode::Force => "force (no lease)",
        PushMode::Plain => "plain",
    };

    let mut pushed = Vec::new();
    for (i, branch) in branches.iter().enumerate() {
        reporter.start(&format!(
            "pushing `{branch}` to `{remote}` [{mode_label}] ({n}/{total})",
            n = i + 1,
        ));
        match git.push(branch, mode, remote) {
            Ok(()) => reporter.ok(""),
            Err(e) => {
                reporter.fail(&e.to_string());
                return Err(e);
            }
        }
        let sha = git.rev_parse(branch).ok();
        if let Some(sha) = sha {
            if let Some(pr) = state.train_mut(&train_name)?.prs.get_mut(branch) {
                pr.last_pushed_sha = Some(sha);
            }
        }
        pushed.push(branch.clone());
    }
    state::save(repo_root, &state)?;
    Ok(PushSummary {
        train: train_name,
        pushed,
        mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::git::RebaseOutcome;
    use crate::report::{NullReporter, RecordingReporter};
    use crate::state::{PrInfo, StateFile, Train};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    struct FakeGit {
        tips: BTreeMap<String, String>,
        pushes: RefCell<Vec<(String, PushMode, String)>>,
    }

    impl GitRunner for FakeGit {
        fn current_branch(&self) -> Result<String> {
            Ok("a".into())
        }
        fn branch_exists(&self, name: &str) -> Result<bool> {
            Ok(self.tips.contains_key(name))
        }
        fn checkout(&self, _b: &str) -> Result<()> {
            Ok(())
        }
        fn rev_parse(&self, rev: &str) -> Result<String> {
            self.tips
                .get(rev)
                .cloned()
                .ok_or_else(|| Error::UnknownBranch(rev.to_string()))
        }
        fn rebase_onto(
            &self,
            _b: &str,
            _o: &str,
            _u: &str,
        ) -> Result<RebaseOutcome> {
            unreachable!()
        }
        fn rebase_abort(&self) -> Result<()> {
            Ok(())
        }
        fn push(&self, branch: &str, mode: PushMode, remote: &str) -> Result<()> {
            self.pushes
                .borrow_mut()
                .push((branch.into(), mode, remote.into()));
            Ok(())
        }
        fn fetch(&self, _r: &str) -> Result<()> {
            Ok(())
        }
        fn ahead_behind(&self, _b: &str, _u: &str) -> Result<Option<(u32, u32)>> {
            Ok(None)
        }
    }

    fn setup() -> (TempDir, FakeGit) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();

        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.branches = vec!["a".into(), "b".into()];
        t.prs.insert(
            "a".into(),
            PrInfo {
                number: 1,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        state::save(tmp.path(), &state).unwrap();

        let git = FakeGit {
            tips: [("main", "M"), ("a", "A1"), ("b", "B1")]
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            pushes: RefCell::new(Vec::new()),
        };
        (tmp, git)
    }

    #[test]
    fn pushes_every_branch_with_force_with_lease() {
        let (tmp, git) = setup();
        let summary = run(
            tmp.path(),
            &git,
            &mut NullReporter,
            None,
            PushMode::ForceWithLease,
            "origin",
        )
        .unwrap();
        assert_eq!(summary.pushed, vec!["a", "b"]);
        let pushes = git.pushes.borrow().clone();
        assert_eq!(
            pushes,
            vec![
                ("a".into(), PushMode::ForceWithLease, "origin".into()),
                ("b".into(), PushMode::ForceWithLease, "origin".into()),
            ]
        );
    }

    #[test]
    fn force_mode_passes_unconditional_force_to_git() {
        let (tmp, git) = setup();
        run(
            tmp.path(),
            &git,
            &mut NullReporter,
            None,
            PushMode::Force,
            "origin",
        )
        .unwrap();
        let pushes = git.pushes.borrow().clone();
        assert!(pushes.iter().all(|(_, m, _)| *m == PushMode::Force));
    }

    #[test]
    fn plain_mode_passes_no_force_flag_to_git() {
        let (tmp, git) = setup();
        run(
            tmp.path(),
            &git,
            &mut NullReporter,
            None,
            PushMode::Plain,
            "origin",
        )
        .unwrap();
        let pushes = git.pushes.borrow().clone();
        assert!(pushes.iter().all(|(_, m, _)| *m == PushMode::Plain));
    }

    #[test]
    fn updates_last_pushed_sha_for_branches_with_prs() {
        let (tmp, git) = setup();
        run(
            tmp.path(),
            &git,
            &mut NullReporter,
            None,
            PushMode::ForceWithLease,
            "origin",
        )
        .unwrap();
        let state = state::load(tmp.path()).unwrap();
        let train = state.train("t").unwrap();
        assert_eq!(
            train.prs.get("a").unwrap().last_pushed_sha.as_deref(),
            Some("A1")
        );
    }

    #[test]
    fn emits_one_progress_step_per_branch() {
        let (tmp, git) = setup();
        let mut rep = RecordingReporter::new();
        run(
            tmp.path(),
            &git,
            &mut rep,
            None,
            PushMode::ForceWithLease,
            "origin",
        )
        .unwrap();
        assert_eq!(rep.events.len(), 2);
        assert!(rep.events[0].contains("pushing `a`"));
        assert!(rep.events[0].contains("force-with-lease"));
        assert!(rep.events[0].contains("(1/2)"));
        assert!(rep.events[0].ends_with("ok"));
        assert!(rep.events[1].contains("pushing `b`"));
        assert!(rep.events[1].contains("(2/2)"));
    }

    #[test]
    fn force_mode_status_label_in_progress() {
        let (tmp, git) = setup();
        let mut rep = RecordingReporter::new();
        run(
            tmp.path(),
            &git,
            &mut rep,
            None,
            PushMode::Force,
            "origin",
        )
        .unwrap();
        assert!(
            rep.events[0].contains("force (no lease)"),
            "expected force label, got: {}",
            rep.events[0]
        );
    }
}
