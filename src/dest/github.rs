//! GitHub Actions secrets, set with the `gh` CLI in the repository `origin`
//! points to.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::repo;

/// A secret in the map: `github = { secret = "X", environment = "production" }`.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    /// The secret's name.
    pub secret: String,
    /// A GitHub Environment. Absent: a repository secret.
    #[serde(default)]
    pub environment: Option<String>,
}

impl Secret {
    /// Check the names against GitHub's rules, and refuse an environment
    /// that `gh` could read as a flag.
    pub fn validate(&self) -> Result<()> {
        validate_secret_name(&self.secret)?;
        if let Some(env) = &self.environment {
            validate_environment(env)?;
        }
        Ok(())
    }

    /// Shown to the user: `<environment>/<NAME>`, or `repo/<NAME>`.
    pub fn label(&self) -> String {
        match &self.environment {
            Some(env) => format!("{env}/{}", self.secret),
            None => format!("repo/{}", self.secret),
        }
    }

    /// Which secret on GitHub this is. GitHub ignores case in secret and
    /// environment names, and an environment may be called `repo`.
    pub fn identity(&self) -> String {
        let secret = self.secret.to_ascii_uppercase();
        match &self.environment {
            Some(env) => format!("env/{}/{secret}", env.to_ascii_uppercase()),
            None => format!("repo/{secret}"),
        }
    }
}

/// Set one secret. The value goes over stdin, never on the command line.
/// The host is explicit because `gh` otherwise honours `GH_HOST`.
pub fn set_secret(repo: &str, dest: &Secret, value: &[u8]) -> Result<()> {
    let mut command = Command::new("gh");
    let repo = format!("github.com/{repo}");
    command.args(["secret", "set", &dest.secret, "--repo", &repo]);
    if let Some(env) = &dest.environment {
        command.args(["--env", env]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running gh (is the GitHub CLI installed and logged in?)")?;
    child
        .stdin
        .take()
        .context("opening gh's stdin")?
        .write_all(value)
        .context("sending the value to gh")?;
    let out = child.wait_with_output().context("waiting for gh")?;
    if !out.status.success() {
        bail!(
            "gh secret set {} failed: {}",
            dest.label(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// GitHub's secret naming rules: letters, digits and `_`, not starting with
/// a digit or `GITHUB_`.
fn validate_secret_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid || name.to_ascii_uppercase().starts_with("GITHUB_") {
        bail!("{name:?} isn't a valid GitHub secret name");
    }
    Ok(())
}

/// A conservative subset of GitHub environment names.
fn validate_environment(env: &str) -> Result<()> {
    let valid = !env.is_empty()
        && !env.starts_with('-')
        && env
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !valid {
        bail!("{env:?} isn't a supported GitHub environment name");
    }
    Ok(())
}

/// `owner/name` of the `origin` remote, which must be on github.com. GitHub
/// targets only ever go here, never to a repository the map names.
pub fn origin(root: &Path) -> Result<String> {
    parse_remote(&repo::origin_url(root)?)
}

/// `owner/name` from a github.com SSH or HTTPS remote URL. Errors never
/// include the URL, which may hold credentials.
fn parse_remote(url: &str) -> Result<String> {
    let url = url.trim();
    // The URL stays out of this message: it can hold a token.
    let not_github = "origin isn't a github.com repository (see `git remote get-url origin`)";
    let rest = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://") {
        let (authority, path) = rest.split_once('/').context(not_github)?;
        // These end the host in a URL, so anything after them isn't the host.
        if authority.contains(['?', '#', '\\']) {
            bail!(not_github);
        }
        let host = authority
            .split_once('@')
            .map_or(authority, |(_, host)| host);
        if host != "github.com" {
            bail!(not_github);
        }
        path
    } else {
        bail!(not_github);
    };
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/').context(not_github)?;
    let ok = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    if !ok(owner) || !ok(name) {
        bail!(not_github);
    }
    Ok(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names() {
        assert!(validate_secret_name("ENV_FILE_CONTENT").is_ok());
        assert!(validate_secret_name("_X1").is_ok());
        assert!(validate_secret_name("GITHUB_TOKEN").is_err());
        assert!(validate_secret_name("1X").is_err());
        assert!(validate_secret_name("A-B").is_err());
    }

    #[test]
    fn environments() {
        assert!(validate_environment("production").is_ok());
        assert!(validate_environment("--repo").is_err());
        assert!(validate_environment("a b").is_err());
    }

    #[test]
    fn identity_ignores_case_and_separates_kinds() {
        let dest = |secret: &str, environment: Option<&str>| Secret {
            secret: secret.into(),
            environment: environment.map(str::to_string),
        };
        assert_eq!(
            dest("TOKEN", None).identity(),
            dest("token", None).identity()
        );
        assert_eq!(
            dest("X", Some("Prod")).identity(),
            dest("x", Some("prod")).identity()
        );
        assert_ne!(
            dest("X", None).identity(),
            dest("X", Some("repo")).identity()
        );
    }

    #[test]
    fn remotes() {
        for url in [
            "git@github.com:Owner/repo.git",
            "ssh://git@github.com/Owner/repo.git",
            "https://github.com/Owner/repo",
            "https://github.com/Owner/repo.git/",
            "https://x-token@github.com/Owner/repo.git",
            "https://user:token@github.com/Owner/repo.git",
        ] {
            assert_eq!(parse_remote(url).unwrap(), "Owner/repo", "{url}");
        }
        for url in [
            "git@gitlab.com:Owner/repo.git",
            "https://github.com.evil.example/Owner/repo",
            "https://github.com/Owner",
            "https://github.com/Owner/repo/extra",
            "https://github.com/-Owner/repo",
        ] {
            assert!(parse_remote(url).is_err(), "{url}");
        }
    }

    #[test]
    fn another_host_cannot_pose_as_github() {
        for url in [
            "https://evil.example/@github.com/acme/widgets",
            "https://evil.example?@github.com/acme/widgets",
            "https://evil.example#@github.com/acme/widgets",
            "https://evil.example\\@github.com/acme/widgets",
            "https://github.com@evil.example/acme/widgets",
            "https://a@evil.example@github.com/acme/widgets",
        ] {
            assert!(parse_remote(url).is_err(), "{url}");
        }
    }

    #[test]
    fn remote_errors_hide_the_url() {
        for url in [
            "https://user:s3cret@gitlab.example.com/a/b.git",
            "https://user:s3cret@github.com/only-owner",
            "s3cret",
        ] {
            let err = format!("{:#}", parse_remote(url).unwrap_err());
            assert!(!err.contains("s3cret"), "{err}");
            assert!(err.contains("isn't a github.com repository"), "{err}");
        }
    }
}
