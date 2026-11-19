//! Destinations `push` sends rendered values to. A target's destination is
//! either a local file (written by `pull`) or one of these. Each kind is its
//! own module; GitHub is the only one so far.
//!
//! To add a kind: a module here with a destination type parsed from the map
//! (and its key in `map::RawTarget`), a variant of `Remote`, and whatever
//! per-repository context it needs in `Remotes`, resolved before any
//! credential is asked for.

mod github;
mod state;

use std::path::Path;

use anyhow::Result;

pub use github::Secret as GithubSecret;
pub use state::State;

/// A remote destination named in the map.
#[derive(Clone, Debug)]
pub enum Remote {
    Github(github::Secret),
}

impl Remote {
    /// Check what the map gave, by the destination's own naming rules.
    pub fn validate(&self) -> Result<()> {
        match self {
            Remote::Github(secret) => secret.validate(),
        }
    }

    /// Shown to the user, e.g. `github:production/ENV_FILE_CONTENT`.
    pub fn label(&self) -> String {
        match self {
            Remote::Github(secret) => format!("github:{}", secret.label()),
        }
    }

    /// Which destination this is, for spotting two targets that name the
    /// same one. Follows the destination's own rules on case and the like.
    pub fn identity(&self) -> String {
        match self {
            Remote::Github(secret) => format!("github:{}", secret.identity()),
        }
    }
}

/// What a run needs to reach the remote destinations it uses, resolved from
/// the repository, never from the map: for GitHub, the `origin` repository.
pub struct Remotes {
    github: Option<String>,
}

impl Remotes {
    /// Resolve only what `remotes` need, so a map with no GitHub targets
    /// doesn't need a GitHub origin.
    pub fn resolve<'a>(
        root: &Path,
        remotes: impl IntoIterator<Item = &'a Remote>,
    ) -> Result<Remotes> {
        let mut github = None;
        for remote in remotes {
            match remote {
                Remote::Github(_) if github.is_none() => github = Some(github::origin(root)?),
                Remote::Github(_) => {}
            }
        }
        Ok(Remotes { github })
    }

    /// One line per kind in use, saying where its targets go.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(repo) = &self.github {
            lines.push(format!("github targets go to {repo} (from origin)"));
        }
        lines
    }

    /// The key `State` records this destination's last push under.
    pub fn state_key(&self, remote: &Remote) -> String {
        match remote {
            Remote::Github(secret) => {
                format!("github:{}#{}", self.github_repo(), secret.identity())
            }
        }
    }

    /// Send `value` to the destination, replacing what's there.
    pub fn set(&self, remote: &Remote, value: &[u8]) -> Result<()> {
        match remote {
            Remote::Github(secret) => github::set_secret(self.github_repo(), secret, value),
        }
    }

    /// The `owner/name` GitHub targets go to. Panics if `resolve` saw none.
    fn github_repo(&self) -> &str {
        self.github
            .as_deref()
            .expect("Remotes::resolve was given every remote it's used for")
    }
}
