use anyhow::{bail, Result};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl RepoIdentity {
    pub fn id(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.repo)
    }
}

#[derive(Debug, Error)]
pub enum RemoteParseError {
    #[error("unsupported or unrecognised remote URL: {}", .0.escape_debug())]
    Unrecognised(String),
    #[error("refusing repository `{}`: each part of host/owner/repo must be a plain name (no empty, `.` or `..` segment, control or invisible character, `<`, `>` or `\\`)", .0.escape_debug())]
    Unsafe(String),
}

pub fn parse_remote_url(url: &str) -> Result<RepoIdentity, RemoteParseError> {
    let url = url.trim();
    let url = url.strip_suffix(".git").unwrap_or(url);

    if let Some(rest) = url.strip_prefix("git@") {
        return parse_scp_like(rest);
    }

    if url.starts_with("ssh://") {
        let rest = url.strip_prefix("ssh://").unwrap();
        let (auth_host, path) = rest
            .split_once('/')
            .ok_or_else(|| RemoteParseError::Unrecognised(url.to_string()))?;
        let host = auth_host
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(auth_host);
        let host = host.split(':').next().unwrap_or(host);
        return parse_host_owner_repo(host, path);
    }

    if url.starts_with("https://") || url.starts_with("http://") {
        let without_scheme = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap();
        let (host, path) = without_scheme
            .split_once('/')
            .ok_or_else(|| RemoteParseError::Unrecognised(url.to_string()))?;
        return parse_host_owner_repo(host, path);
    }

    Err(RemoteParseError::Unrecognised(url.to_string()))
}

fn parse_scp_like(rest: &str) -> Result<RepoIdentity, RemoteParseError> {
    let (host, path) = rest
        .split_once(':')
        .ok_or_else(|| RemoteParseError::Unrecognised(rest.to_string()))?;
    parse_host_owner_repo(host, path)
}

fn parse_host_owner_repo(host: &str, path: &str) -> Result<RepoIdentity, RemoteParseError> {
    let path = path.trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return Err(RemoteParseError::Unrecognised(path.to_string()));
    }
    let repo = parts[parts.len() - 1].to_string();
    let owner = parts[..parts.len() - 1].join("/");
    let identity = RepoIdentity {
        host: host.to_string(),
        owner,
        repo,
    };
    check_identity(&identity)?;
    Ok(identity)
}

/// Refuse an identity that would not stay a plain path under the store, or
/// that prints as something else: its parts become `store/<host>/<owner>/<repo>`
/// and its id is shown to the user and to agents. So no empty, `.` or `..`
/// segment, and no control, format (invisible or bidi) character, `<` or `>`.
fn check_identity(identity: &RepoIdentity) -> Result<(), RemoteParseError> {
    let bad = |part: &str| {
        part.split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
            || part.chars().any(|c| {
                c.is_control() || crate::tasks::format_char(c) || matches!(c, '<' | '>' | '\\')
            })
    };
    if [&identity.host, &identity.owner, &identity.repo]
        .iter()
        .any(|part| bad(part))
    {
        return Err(RemoteParseError::Unsafe(identity.id()));
    }
    Ok(())
}

pub fn parse_remote_url_anyhow(url: &str) -> Result<RepoIdentity> {
    parse_remote_url(url).map_err(|e| anyhow::anyhow!(e))
}

pub fn identity_from_id(id: &str) -> Result<RepoIdentity> {
    let parts: Vec<&str> = id.split('/').collect();
    if parts.len() < 3 {
        bail!("invalid repo id (expected host/owner/repo): {id}");
    }
    let repo = parts[parts.len() - 1].to_string();
    let owner = parts[parts.len() - 2].to_string();
    let host = parts[..parts.len() - 2].join("/");
    let identity = RepoIdentity { host, owner, repo };
    check_identity(&identity)?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_github() {
        let id = parse_remote_url("https://github.com/acme/widget.git").unwrap();
        assert_eq!(id.id(), "github.com/acme/widget");
    }

    #[test]
    fn parses_git_scp() {
        let id = parse_remote_url("git@github.com:acme/widget.git").unwrap();
        assert_eq!(id.id(), "github.com/acme/widget");
    }

    #[test]
    fn parses_gitlab_https() {
        let id = parse_remote_url("https://gitlab.com/group/subgroup/myrepo").unwrap();
        assert_eq!(id.host, "gitlab.com");
        assert_eq!(id.owner, "group/subgroup");
        assert_eq!(id.repo, "myrepo");
        assert_eq!(id.id(), "gitlab.com/group/subgroup/myrepo");
    }

    #[test]
    fn parses_ssh_url() {
        let id = parse_remote_url("ssh://git@github.com/acme/widget").unwrap();
        assert_eq!(id.id(), "github.com/acme/widget");
    }

    #[test]
    fn refuses_identities_that_leave_the_store_or_hide_text() {
        for url in [
            "ssh://evil.com/../../out/o/r",
            "https://evil.example/../../assistant/.claude/skills/pwn.git",
            "git@evil:../../assistant/.claude/skills/pwn.git",
            "https://h/a/./r",
            "https://../a/r",
            "https://h/a/..",
            "ssh://evil.com/o/r\u{1b}[31mred",
            "https://h/o\u{202e}x/r",
            "https://h/o/r\u{200b}",
            "https://h/<o>/r",
            "https://h/o\\..\\x/r",
        ] {
            let err = parse_remote_url(url).unwrap_err().to_string();
            assert!(err.contains("refusing repository"), "{url}: {err}");
            assert!(!err.contains('\u{1b}'), "{url}: {err}");
        }
        for id in ["h.tld/../r", "h.tld/o/..", "../o/r", "h.tld/o\u{1b}/r"] {
            assert!(identity_from_id(id).is_err(), "{id}");
        }
        // Dots inside a name are fine: `.github` is a real repository.
        let id = parse_remote_url("https://github.com/acme/.github.git").unwrap();
        assert_eq!(id.id(), "github.com/acme/.github");
        assert!(identity_from_id("github.com/a.b/c..d").is_ok());
    }
}
