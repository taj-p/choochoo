//! User configuration: `~/.config/choochoo/config.toml`.
//!
//! choochoo works with no config file at all — that's the historical
//! local-only behaviour. A config file says where train state should live
//! when you want it shared between machines, and carries any per-repository
//! settings:
//!
//! ```toml
//! [store]
//! repo = "git@github.com:you/choochoo-state.git"
//! branch = "main"   # optional
//!
//! [repo."https://github.com/Canva/canva"]
//! base = "master"
//! ```
//!
//! ## Why `[repo]` keys are normalized
//!
//! A person writing config has a URL to hand, not choochoo's internal
//! identity for their repo — and which URL depends on where they copied it
//! from. So every key goes through [`crate::repoid::from_config_key`] at
//! parse time and ends up as the same `host/owner/repo` string the `origin`
//! URL resolves to. Writing the address-bar URL, the `git remote -v` URL, or
//! the bare key all work and all mean the same repository.
//!
//! ## Why the paths are resolved by hand
//!
//! The obvious move is the `dirs` crate, but `dirs::config_dir()` returns
//! `~/Library/Application Support` on macOS. Someone whose devboxes are a
//! mix of macOS and Linux would then have their config in two different
//! places — precisely the confusion shared state exists to remove. So we
//! resolve `$XDG_CONFIG_HOME` / `$HOME/.config` directly and get the same
//! path everywhere.
//!
//! ## Why the environment is a struct
//!
//! Everything here takes an [`Env`] rather than reading `std::env`.
//! `cargo test` runs many tests per process on many threads, and
//! `std::env::set_var` is process-global (and unsound under concurrency —
//! it's `unsafe` as of edition 2024), so tests that override env vars
//! would race each other. Passing an [`Env`] literal has no such hazard,
//! and it means a unit test physically cannot read the developer's real
//! config.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// Value of `CHOOCHOO_CONFIG` that means "ignore any config file and stay
/// local-only". Spelled out rather than using the empty string so it reads
/// clearly in a shell: `CHOOCHOO_CONFIG=none choo list`.
pub const CONFIG_NONE: &str = "none";

/// Base branch a new train sits on when nothing else says otherwise —
/// no `--base`, and no `[repo."..."] base` for this repository.
pub const DEFAULT_BASE: &str = "main";

/// Every environment input the config layer needs, captured in one place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `CHOOCHOO_CONFIG` — an explicit config path, or [`CONFIG_NONE`].
    pub config: Option<OsString>,
    /// `XDG_CONFIG_HOME`
    pub xdg_config_home: Option<OsString>,
    /// `XDG_DATA_HOME`
    pub xdg_data_home: Option<OsString>,
    /// `HOME`
    pub home: Option<OsString>,
    /// `CHOOCHOO_STORE_DIR` — overrides where the store clone is kept.
    pub store_dir: Option<OsString>,
    /// `CHOOCHOO_NO_SYNC` — set to anything but `0`/empty to skip syncing.
    pub no_sync: bool,
}

impl Env {
    /// Read the real process environment. The only place in the crate that
    /// touches `std::env` for configuration.
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        Self {
            config: var("CHOOCHOO_CONFIG"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            xdg_data_home: var("XDG_DATA_HOME"),
            home: var("HOME"),
            store_dir: var("CHOOCHOO_STORE_DIR"),
            no_sync: var("CHOOCHOO_NO_SYNC")
                .map(|v| v != "0")
                .unwrap_or(false),
        }
    }
}

/// Where the config file should be read from, and how strict we are about
/// it being there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Named explicitly via `CHOOCHOO_CONFIG`. A missing file is an
    /// **error**: someone asked for this file specifically, and silently
    /// falling back to local-only would hide the mistake.
    Explicit(PathBuf),
    /// The conventional location. A missing file is normal and means
    /// local-only.
    Conventional(PathBuf),
    /// Nowhere to look — `CHOOCHOO_CONFIG=none`, or no `HOME` at all.
    None,
}

/// Resolve the config file location.
///
/// Precedence: `CHOOCHOO_CONFIG` > `$XDG_CONFIG_HOME/choochoo/config.toml`
/// > `$HOME/.config/choochoo/config.toml`. Pure — performs no IO.
pub fn source(env: &Env) -> Source {
    if let Some(explicit) = &env.config {
        if explicit == CONFIG_NONE {
            return Source::None;
        }
        return Source::Explicit(PathBuf::from(explicit));
    }
    if let Some(xdg) = &env.xdg_config_home {
        return Source::Conventional(
            Path::new(xdg).join("choochoo").join("config.toml"),
        );
    }
    if let Some(home) = &env.home {
        return Source::Conventional(
            Path::new(home)
                .join(".config")
                .join("choochoo")
                .join("config.toml"),
        );
    }
    Source::None
}

/// Directory holding the store clone.
///
/// `$XDG_DATA_HOME/choochoo/store`, else `~/.local/share/choochoo/store`.
/// Deliberately a *data* directory rather than a cache directory: when the
/// network is down this clone can hold the only copy of a train, so a cache
/// cleaner must not be entitled to delete it. Pure — performs no IO.
pub fn store_dir(env: &Env) -> Option<PathBuf> {
    if let Some(explicit) = &env.store_dir {
        return Some(PathBuf::from(explicit));
    }
    if let Some(xdg) = &env.xdg_data_home {
        return Some(Path::new(xdg).join("choochoo").join("store"));
    }
    env.home.as_ref().map(|home| {
        Path::new(home)
            .join(".local")
            .join("share")
            .join("choochoo")
            .join("store")
    })
}

/// Load the config described by `env`. An absent conventional config yields
/// [`Config::default`] — the local-only mode, which is not an error.
pub fn load(env: &Env) -> Result<Config> {
    match source(env) {
        Source::None => Ok(Config::default()),
        Source::Conventional(path) if !path.exists() => Ok(Config::default()),
        Source::Conventional(path) | Source::Explicit(path) => load_from(&path),
    }
}

/// Load and parse a specific config file. A missing file *is* an error
/// here; callers who want the lenient behaviour go through [`load`].
pub fn load_from(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path).map_err(|e| Error::Config {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    parse(&text, path)
}

/// Parse config TOML. Split out from [`load_from`] so it can be tested
/// without touching the filesystem.
pub fn parse(text: &str, path: &Path) -> Result<Config> {
    let mut config: Config = toml::from_str(text).map_err(|e| Error::Config {
        path: path.to_path_buf(),
        // `toml`'s Display already includes the line/column and a snippet;
        // trim the trailing newline so it sits on our one-line message.
        reason: e.to_string().trim_end().to_string(),
    })?;
    config.normalize_repo_keys(path)?;
    config.validate(path)?;
    Ok(config)
}

/// Parsed `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Absent means local-only: state stays in `.git/choochoo/state.json`.
    #[serde(default)]
    pub store: Option<StoreConfig>,
    /// Per-repository settings from `[repo."<url>"]` tables.
    ///
    /// Keyed by [`crate::repoid`] identity, *not* by the spelling in the
    /// file — see the module docs. Empty is the common case and costs
    /// nothing: no `[repo]` table means no git call to identify the repo.
    #[serde(default, rename = "repo")]
    pub repos: BTreeMap<String, RepoConfig>,
}

/// A `[repo."<url>"]` table: settings that apply only to one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Base branch `choo init` uses here when `--base` is not given.
    ///
    /// This is a *default for new trains*, not a property of the repo:
    /// changing it never moves a train that already exists, since each
    /// train records the base it was created with.
    #[serde(default)]
    pub base: Option<String>,
}

/// The `[store]` table: a git repo that holds choochoo's train metadata.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    /// Anything `git clone` accepts. Normally a private GitHub repo you own.
    pub repo: String,
    /// Branch within the store repo.
    #[serde(default = "default_branch")]
    pub branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

impl Config {
    /// Rewrite `[repo."..."]` keys from whatever the user typed into the
    /// identity [`crate::repoid::from_url`] derives from a remote.
    ///
    /// Two spellings of one repository is a hard error rather than a
    /// last-one-wins merge: the file would read as though both entries
    /// applied, and the one being ignored is exactly the sort of thing
    /// someone would spend an afternoon on.
    fn normalize_repo_keys(&mut self, path: &Path) -> Result<()> {
        if self.repos.is_empty() {
            return Ok(());
        }
        let bad = |reason: String| Error::Config {
            path: path.to_path_buf(),
            reason,
        };
        let mut normalized = BTreeMap::new();
        let mut spellings: BTreeMap<String, String> = BTreeMap::new();
        for (spelling, repo) in std::mem::take(&mut self.repos) {
            let key = crate::repoid::from_config_key(&spelling).ok_or_else(|| {
                bad(format!(
                    "`[repo.\"{spelling}\"]` is not a repository URL; use the \
                     URL of the repo's `origin`, e.g. \
                     `[repo.\"https://github.com/owner/name\"]`"
                ))
            })?;
            if let Some(previous) = spellings.insert(key.clone(), spelling.clone()) {
                return Err(bad(format!(
                    "`[repo.\"{previous}\"]` and `[repo.\"{spelling}\"]` are the \
                     same repository (`{key}`); keep one of them"
                )));
            }
            normalized.insert(key, repo);
        }
        self.repos = normalized;
        Ok(())
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if let Some(store) = &self.store {
            if store.repo.trim().is_empty() {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    reason: "`store.repo` cannot be empty".into(),
                });
            }
            if store.branch.trim().is_empty() {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    reason: "`store.branch` cannot be empty".into(),
                });
            }
        }
        for (key, repo) in &self.repos {
            if repo.base.as_ref().is_some_and(|b| b.trim().is_empty()) {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    reason: format!("`repo.\"{key}\".base` cannot be empty"),
                });
            }
        }
        Ok(())
    }

    /// Configured base branch for the repository identified by `key`, if the
    /// user set one. `key` comes from [`crate::repoid`], so it is already in
    /// the same normalized form as the map.
    pub fn base_for(&self, key: &str) -> Option<&str> {
        self.repos.get(key)?.base.as_deref()
    }

    /// The configured store, if any.
    ///
    /// Note that `--no-sync` / `CHOOCHOO_NO_SYNC` deliberately does *not*
    /// hide this. Once state is shared, `.git/choochoo/state.json` is no
    /// longer the truth, so falling back to it would show the user an empty
    /// or stale set of trains. Instead the store is opened *offline*: reads
    /// come from the existing clone and writes commit locally, publishing on
    /// the next synced command.
    pub fn store(&self) -> Option<&StoreConfig> {
        self.store.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    fn p() -> PathBuf {
        PathBuf::from("/cfg/config.toml")
    }

    /// The most important test in this module: an empty environment must
    /// resolve to no config at all. If this ever regresses, unit tests on a
    /// developer machine could start reading their real config.
    #[test]
    fn empty_env_finds_nothing_and_stays_local() {
        assert_eq!(source(&Env::default()), Source::None);
        assert_eq!(load(&Env::default()).unwrap(), Config::default());
        assert!(load(&Env::default()).unwrap().store.is_none());
        assert_eq!(store_dir(&Env::default()), None);
    }

    #[test]
    fn explicit_config_wins_over_xdg_and_home() {
        let env = Env {
            config: os("/explicit.toml"),
            xdg_config_home: os("/xdg"),
            home: os("/home/u"),
            ..Default::default()
        };
        assert_eq!(
            source(&env),
            Source::Explicit(PathBuf::from("/explicit.toml"))
        );
    }

    #[test]
    fn config_none_forces_local_only() {
        let env = Env {
            config: os(CONFIG_NONE),
            xdg_config_home: os("/xdg"),
            home: os("/home/u"),
            ..Default::default()
        };
        assert_eq!(source(&env), Source::None);
    }

    #[test]
    fn xdg_config_home_wins_over_home() {
        let env = Env {
            xdg_config_home: os("/xdg"),
            home: os("/home/u"),
            ..Default::default()
        };
        assert_eq!(
            source(&env),
            Source::Conventional(PathBuf::from("/xdg/choochoo/config.toml"))
        );
    }

    /// The path the user actually asked for, on every platform — note this
    /// is `~/.config` even on macOS, unlike `dirs::config_dir()`.
    #[test]
    fn home_falls_back_to_dot_config() {
        let env = Env {
            home: os("/home/u"),
            ..Default::default()
        };
        assert_eq!(
            source(&env),
            Source::Conventional(PathBuf::from(
                "/home/u/.config/choochoo/config.toml"
            ))
        );
    }

    #[test]
    fn store_dir_precedence() {
        assert_eq!(
            store_dir(&Env {
                store_dir: os("/explicit"),
                xdg_data_home: os("/xdg"),
                home: os("/home/u"),
                ..Default::default()
            }),
            Some(PathBuf::from("/explicit"))
        );
        assert_eq!(
            store_dir(&Env {
                xdg_data_home: os("/xdg"),
                home: os("/home/u"),
                ..Default::default()
            }),
            Some(PathBuf::from("/xdg/choochoo/store"))
        );
        assert_eq!(
            store_dir(&Env {
                home: os("/home/u"),
                ..Default::default()
            }),
            Some(PathBuf::from("/home/u/.local/share/choochoo/store"))
        );
    }

    #[test]
    fn absent_conventional_config_is_not_an_error() {
        let env = Env {
            xdg_config_home: os("/definitely/not/a/real/path"),
            ..Default::default()
        };
        assert_eq!(load(&env).unwrap(), Config::default());
    }

    /// The opposite: if you named a file, we don't quietly ignore it.
    #[test]
    fn absent_explicit_config_is_an_error() {
        let env = Env {
            config: os("/definitely/not/a/real/config.toml"),
            ..Default::default()
        };
        assert!(matches!(load(&env), Err(Error::Config { .. })));
    }

    #[test]
    fn empty_config_parses_to_local_only() {
        assert_eq!(parse("", &p()).unwrap(), Config::default());
    }

    #[test]
    fn store_section_parses_with_default_branch() {
        let c = parse("[store]\nrepo = \"git@github.com:me/s.git\"\n", &p()).unwrap();
        let store = c.store.unwrap();
        assert_eq!(store.repo, "git@github.com:me/s.git");
        assert_eq!(store.branch, "main");
    }

    #[test]
    fn store_branch_is_overridable() {
        let c = parse(
            "[store]\nrepo = \"u\"\nbranch = \"trains\"\n",
            &p(),
        )
        .unwrap();
        assert_eq!(c.store.unwrap().branch, "trains");
    }

    #[test]
    fn repo_base_is_keyed_by_repo_identity() {
        let c = parse(
            "[repo.\"https://github.com/Canva/canva\"]\nbase = \"master\"\n",
            &p(),
        )
        .unwrap();
        assert_eq!(c.base_for("github.com/canva/canva"), Some("master"));
        assert_eq!(c.base_for("github.com/canva/other"), None);
    }

    /// The point of normalizing: it doesn't matter which URL the user had to
    /// hand, only which repo it names.
    #[test]
    fn every_url_spelling_reaches_the_same_repo() {
        for spelling in [
            "https://github.com/Canva/canva",
            "https://github.com/canva/canva.git",
            "git@github.com:Canva/canva.git",
            "github.com/canva/canva",
        ] {
            let c = parse(&format!("[repo.\"{spelling}\"]\nbase = \"master\"\n"), &p())
                .unwrap();
            assert_eq!(
                c.base_for("github.com/canva/canva"),
                Some("master"),
                "{spelling} did not resolve"
            );
        }
    }

    /// Two spellings of one repo means one of them is silently dead. Say so.
    #[test]
    fn duplicate_repo_spellings_are_rejected() {
        let err = parse(
            "[repo.\"https://github.com/Canva/canva\"]\nbase = \"master\"\n\
             [repo.\"git@github.com:canva/canva.git\"]\nbase = \"main\"\n",
            &p(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("same repository"), "got: {msg}");
        assert!(msg.contains("github.com/canva/canva"), "got: {msg}");
    }

    #[test]
    fn repo_table_without_base_is_allowed() {
        let c = parse("[repo.\"github.com/o/r\"]\n", &p()).unwrap();
        assert_eq!(c.base_for("github.com/o/r"), None);
        assert!(c.repos.contains_key("github.com/o/r"));
    }

    #[test]
    fn blank_repo_base_is_rejected() {
        assert!(matches!(
            parse("[repo.\"github.com/o/r\"]\nbase = \" \"\n", &p()),
            Err(Error::Config { .. })
        ));
    }

    #[test]
    fn unusable_repo_key_is_rejected() {
        let err = parse("[repo.\"\"]\nbase = \"master\"\n", &p()).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
        assert!(err.to_string().contains("repository URL"), "{err}");
    }

    #[test]
    fn unknown_repo_key_is_rejected() {
        assert!(matches!(
            parse("[repo.\"github.com/o/r\"]\nbase0 = \"master\"\n", &p()),
            Err(Error::Config { .. })
        ));
    }

    /// `[repo]` and `[store]` are independent: per-repo settings must work
    /// for someone who has never turned on shared state.
    #[test]
    fn repo_settings_work_without_a_store() {
        let c = parse("[repo.\"github.com/o/r\"]\nbase = \"master\"\n", &p()).unwrap();
        assert!(c.store().is_none());
        assert_eq!(c.base_for("github.com/o/r"), Some("master"));
    }

    #[test]
    fn no_repo_table_means_no_per_repo_settings() {
        let c = parse("[store]\nrepo = \"u\"\n", &p()).unwrap();
        assert!(c.repos.is_empty());
        assert_eq!(c.base_for("github.com/o/r"), None);
    }

    /// A typo that silently left sync switched off would look exactly like
    /// choochoo losing a train, so unknown keys are rejected.
    #[test]
    fn unknown_key_is_rejected() {
        let err = parse("[store]\nrep0 = \"u\"\n", &p()).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
        let msg = err.to_string();
        assert!(msg.contains("config.toml"), "got: {msg}");
    }

    #[test]
    fn unknown_top_level_table_is_rejected() {
        assert!(matches!(
            parse("[sync]\nauto = true\n", &p()),
            Err(Error::Config { .. })
        ));
    }

    #[test]
    fn store_without_repo_is_rejected() {
        assert!(matches!(
            parse("[store]\nbranch = \"main\"\n", &p()),
            Err(Error::Config { .. })
        ));
    }

    #[test]
    fn blank_repo_and_branch_are_rejected() {
        assert!(matches!(
            parse("[store]\nrepo = \"  \"\n", &p()),
            Err(Error::Config { .. })
        ));
        assert!(matches!(
            parse("[store]\nrepo = \"u\"\nbranch = \" \"\n", &p()),
            Err(Error::Config { .. })
        ));
    }

    /// `--no-sync` must not turn a shared repo back into a local one — that
    /// would show the user a stale `state.json` instead of their trains. It
    /// only makes the store offline, which is a decision made further down.
    #[test]
    fn no_sync_does_not_hide_the_store_config() {
        let cfg = parse("[store]\nrepo = \"u\"\n", &p()).unwrap();
        assert!(cfg.store().is_some());
    }

    #[test]
    fn no_store_section_means_no_store() {
        assert!(parse("", &p()).unwrap().store().is_none());
    }

    #[test]
    fn malformed_toml_names_the_file() {
        let err = parse("[store\nrepo =", &p()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/cfg/config.toml"), "got: {msg}");
    }
}
