//! Repository identity: turning a git remote URL into a stable key.
//!
//! Shared state is keyed by *which repository* a train belongs to, and that
//! key has to come out the same on every machine. A local filesystem path
//! won't do — the same repo is `~/src/thing` on one devbox and
//! `/workspace/thing` on another. The `origin` URL is the thing they agree
//! on, so it's what we normalize.
//!
//! Every form git accepts collapses to `host/owner/repo`:
//!
//! | URL | key |
//! |---|---|
//! | `git@github.com:Owner/Repo.git` | `github.com/owner/repo` |
//! | `ssh://git@github.com:22/owner/repo` | `github.com/owner/repo` |
//! | `https://user:token@github.com/owner/repo.git` | `github.com/owner/repo` |
//! | `git://github.com/owner/repo.git` | `github.com/owner/repo` |
//! | `/tmp/xyz/bare.git` | `local/bare-1f2e3d4c` |
//!
//! Case is folded because GitHub is case-insensitive about owner and repo,
//! so one devbox cloning `Owner/Repo` and another cloning `owner/repo` must
//! still land on the same key.

/// Normalize a git remote URL into a stable, filesystem-safe identity.
///
/// Returns [`None`] only when there is nothing usable in `url` at all.
pub fn from_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // `file://` is just a path with a prefix; treat it as one.
    if let Some(path) = url.strip_prefix("file://") {
        return Some(local_key(path));
    }

    // scp-style `[user@]host:path` — the form git offers by default.
    // Distinguished from a URL by having no `://`, and from a Windows drive
    // letter (`C:\...`) by the colon not being at index 1.
    if !url.contains("://") {
        if let Some((host_part, path)) = url.split_once(':') {
            if host_part.len() > 1 && !path.starts_with('\\') {
                let host = strip_userinfo(host_part);
                return match (clean_host(host), clean_path(path)) {
                    (Some(h), Some(p)) => Some(format!("{h}/{p}")),
                    _ => Some(local_key(url)),
                };
            }
        }
        // A bare local path.
        return Some(local_key(url));
    }

    // A real URL: `scheme://[userinfo@]host[:port]/path`.
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    let (authority, path) = match after_scheme.split_once('/') {
        Some((a, p)) => (a, p),
        // `ssh://host` with no path carries no identity.
        None => return Some(local_key(url)),
    };
    let host = strip_port(strip_userinfo(authority));
    match (clean_host(host), clean_path(path)) {
        (Some(h), Some(p)) => Some(format!("{h}/{p}")),
        _ => Some(local_key(url)),
    }
}

/// Normalize a repository as *written by hand in config.toml* into the same
/// key [`from_url`] derives from a git remote.
///
/// Config is typed by a person, so it accepts whichever spelling they have to
/// hand: the address bar (`https://github.com/Canva/canva`), what `git remote
/// -v` prints (`git@github.com:Canva/canva.git`), or the bare key itself
/// (`github.com/canva/canva`). Only that last form needs help — with no
/// scheme and no colon, [`from_url`] would read it as a local directory — so
/// it gets an `https://` prefix and goes through the same path as everything
/// else. A leading `/` or `.` marks a genuine local path and is left alone.
///
/// Returns [`None`] only when there is nothing usable in `spec` at all.
pub fn from_config_key(spec: &str) -> Option<String> {
    let spec = spec.trim();
    let bare_host_path = !spec.contains("://")
        && !spec.contains(':')
        && !spec.starts_with('/')
        && !spec.starts_with('.')
        && spec.contains('/');
    if bare_host_path {
        return from_url(&format!("https://{spec}"));
    }
    from_url(spec)
}

/// True when `key` is safe to use as a relative path inside the store repo.
///
/// Guards the path join in the store backend: a key is attacker-adjacent
/// data (it comes from a remote URL), so `..` and absolute paths must never
/// survive into a filesystem path.
pub fn is_valid(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && !key.ends_with('/')
        && !key.contains("//")
        && !key.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        && key.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_')
        })
}

/// Strip `user@` / `user:password@` from an authority.
fn strip_userinfo(authority: &str) -> &str {
    match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    }
}

/// Strip `:port`. Left alone for IPv6 literals, which we key as local.
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host;
    }
    match host.split_once(':') {
        Some((h, _)) => h,
        None => host,
    }
}

fn clean_host(host: &str) -> Option<String> {
    let host = host.trim().trim_matches('/').to_ascii_lowercase();
    if host.is_empty() || !host.chars().all(is_key_char) {
        return None;
    }
    Some(host)
}

/// Normalize the path part: strip a leading `/`, a trailing `/`, and a
/// trailing `.git`, then lowercase.
fn clean_path(path: &str) -> Option<String> {
    let path = path.trim().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_matches('/').to_ascii_lowercase();
    if path.is_empty() {
        return None;
    }
    let key = format!("x/{path}");
    if !is_valid(&key) {
        return None;
    }
    Some(path)
}

/// Identity for something that isn't a recognizable hosted repo — a bare
/// local path, a `file://` URL, an oddly-shaped remote. Keyed under
/// `local/` with a hash so two similarly-named paths can't collide.
///
/// This is also the path the hermetic tests take, since their `origin` is a
/// bare repo in a tempdir.
fn local_key(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let base = trimmed
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("repo");
    let base = base.strip_suffix(".git").unwrap_or(base);
    let mut slug: String = base
        .to_ascii_lowercase()
        .chars()
        .map(|c| if is_key_char(c) { c } else { '-' })
        .collect();
    slug = slug.trim_matches(['-', '.']).to_string();
    if slug.is_empty() {
        slug = "repo".to_string();
    }
    slug.truncate(48);
    format!("local/{slug}-{:08x}", fnv1a32(trimmed))
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// FNV-1a, 32-bit. Hand-rolled deliberately: `DefaultHasher`'s output is
/// explicitly not guaranteed stable across Rust releases, and this value is
/// persisted in the store repo forever. A toolchain upgrade must not
/// silently re-key someone's trains.
fn fnv1a32(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str) -> String {
        from_url(url).unwrap_or_else(|| panic!("no key for {url}"))
    }

    #[test]
    fn scp_style_github() {
        assert_eq!(key("git@github.com:owner/repo.git"), "github.com/owner/repo");
        assert_eq!(key("git@github.com:owner/repo"), "github.com/owner/repo");
    }

    #[test]
    fn https_github() {
        assert_eq!(
            key("https://github.com/owner/repo.git"),
            "github.com/owner/repo"
        );
        assert_eq!(key("https://github.com/owner/repo/"), "github.com/owner/repo");
    }

    #[test]
    fn ssh_url_with_port() {
        assert_eq!(
            key("ssh://git@github.com:22/owner/repo.git"),
            "github.com/owner/repo"
        );
    }

    #[test]
    fn git_protocol() {
        assert_eq!(
            key("git://github.com/owner/repo.git"),
            "github.com/owner/repo"
        );
    }

    /// A token in the URL must never make it into the persisted key — that
    /// key gets committed to the store repo.
    #[test]
    fn credentials_are_stripped() {
        let k = key("https://user:ghp_secrettoken@github.com/owner/repo.git");
        assert_eq!(k, "github.com/owner/repo");
        assert!(!k.contains("ghp_"), "token leaked into {k}");
        assert!(!k.contains("user"));
    }

    /// One devbox cloned `Owner/Repo`, another cloned `owner/repo`. They
    /// must share a key or the whole feature silently splits in two.
    #[test]
    fn case_is_folded() {
        assert_eq!(
            key("git@github.com:Owner/Repo.git"),
            key("https://github.com/owner/repo")
        );
        assert_eq!(key("git@GitHub.com:Owner/Repo.git"), "github.com/owner/repo");
    }

    /// The same repo reached over ssh from one box and https from another.
    #[test]
    fn ssh_and_https_agree() {
        assert_eq!(
            key("git@github.com:taj/choochoo.git"),
            key("https://github.com/taj/choochoo.git")
        );
    }

    #[test]
    fn nested_paths_are_preserved_for_other_forges() {
        assert_eq!(
            key("https://gitlab.com/group/sub/repo.git"),
            "gitlab.com/group/sub/repo"
        );
    }

    #[test]
    fn local_paths_get_a_hashed_key() {
        let k = key("/tmp/xyz/bare.git");
        assert!(k.starts_with("local/bare-"), "got {k}");
        assert!(is_valid(&k), "got {k}");
    }

    #[test]
    fn file_url_and_plain_path_agree() {
        assert_eq!(key("file:///tmp/xyz/bare.git"), key("/tmp/xyz/bare.git"));
    }

    /// Two bare repos with the same basename in different directories are
    /// different repos and must not share a key.
    #[test]
    fn local_keys_disambiguate_same_basename() {
        assert_ne!(key("/a/bare.git"), key("/b/bare.git"));
        assert!(key("/a/bare.git").starts_with("local/bare-"));
        assert!(key("/b/bare.git").starts_with("local/bare-"));
    }

    #[test]
    fn local_key_is_stable_across_calls() {
        assert_eq!(key("/tmp/a/b.git"), key("/tmp/a/b.git"));
        assert_eq!(key("/tmp/a/b.git/"), key("/tmp/a/b.git"));
    }

    #[test]
    fn empty_url_has_no_key() {
        assert_eq!(from_url(""), None);
        assert_eq!(from_url("   "), None);
    }

    #[test]
    fn relative_local_path() {
        let k = key("../sibling.git");
        assert!(is_valid(&k), "got {k}");
        assert!(k.starts_with("local/sibling-"), "got {k}");
    }

    #[test]
    fn every_key_is_path_safe() {
        for url in [
            "git@github.com:owner/repo.git",
            "https://github.com/owner/repo",
            "ssh://git@host:22/a/b/c.git",
            "/tmp/x/y.git",
            "file:///tmp/x/y.git",
            "../up.git",
            "git@host:../evil.git",
            "https://host/../../etc/passwd",
            "weird",
            "C:\\repos\\thing",
        ] {
            let k = key(url);
            assert!(is_valid(&k), "unsafe key {k:?} from {url:?}");
        }
    }

    #[test]
    fn traversal_is_rejected_by_is_valid() {
        assert!(!is_valid("../etc"));
        assert!(!is_valid("a/../b"));
        assert!(!is_valid("/abs/path"));
        assert!(!is_valid("a//b"));
        assert!(!is_valid("a/"));
        assert!(!is_valid(""));
        assert!(!is_valid("a/./b"));
        assert!(!is_valid("has space/x"));
        assert!(is_valid("github.com/owner/repo"));
        assert!(is_valid("local/bare-1f2e3d4c"));
    }

    #[test]
    fn windows_drive_path_is_local() {
        let k = key("C:\\repos\\thing");
        assert!(k.starts_with("local/"), "got {k}");
    }

    /// Every spelling someone might reasonably write in config.toml has to
    /// land on the key the `origin` URL produces, or their `[repo]` entry
    /// silently does nothing.
    #[test]
    fn config_keys_agree_with_remote_urls() {
        let expected = key("git@github.com:Canva/canva.git");
        assert_eq!(expected, "github.com/canva/canva");
        for spelling in [
            "https://github.com/Canva/canva",
            "https://github.com/Canva/canva.git",
            "https://github.com/canva/canva/",
            "git@github.com:Canva/canva.git",
            "ssh://git@github.com/canva/canva.git",
            "github.com/Canva/canva",
            "github.com/canva/canva",
        ] {
            assert_eq!(
                from_config_key(spelling).as_deref(),
                Some(expected.as_str()),
                "config key {spelling:?} did not match the remote URL"
            );
        }
    }

    /// A bare local path in config is a local path, not a `host/owner/repo`
    /// that happens to start with a slash — and it must agree with the same
    /// path read back out of `git remote get-url`.
    #[test]
    fn config_keys_agree_for_local_paths() {
        for path in ["/tmp/xyz/bare.git", "../sibling.git", "./sibling.git"] {
            assert_eq!(from_config_key(path).as_deref(), Some(key(path).as_str()));
        }
        assert!(from_config_key("/tmp/xyz/bare.git").unwrap().starts_with("local/"));
    }

    #[test]
    fn empty_config_key_has_no_key() {
        assert_eq!(from_config_key(""), None);
        assert_eq!(from_config_key("   "), None);
    }

    #[test]
    fn fnv1a_is_the_documented_constant() {
        // Known FNV-1a 32-bit vectors; pins the hash so a refactor can't
        // silently re-key everyone's trains.
        assert_eq!(fnv1a32(""), 0x811c_9dc5);
        assert_eq!(fnv1a32("a"), 0xe40c_292c);
        assert_eq!(fnv1a32("foobar"), 0xbf9c_f968);
    }
}
