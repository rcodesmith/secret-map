//! What `push` last sent to each destination. Some destinations can't be read
//! back (GitHub secrets, for one), so a salted digest of each pushed value is
//! kept in `.git/secretmap/push-state.toml` and unchanged values are skipped.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::repo;

/// The on-disk form of `push-state.toml`.
#[derive(Serialize, Deserialize)]
struct StateFile {
    /// Random, per clone, hex-encoded; stops digests being matched against
    /// guessed values or compared across clones.
    salt: String,
    /// State key -> digest of the value last pushed there.
    #[serde(default)]
    pushed: BTreeMap<String, String>,
}

/// The push state of this clone, loaded from (and saved to) the git directory.
pub struct State {
    path: PathBuf,
    file: StateFile,
}

impl State {
    /// Load the state from `git_dir`, or start an empty one with a new salt.
    pub fn load(git_dir: &Path) -> Result<State> {
        let path = git_dir.join("secretmap").join("push-state.toml");
        let file = if path.exists() {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            let mut salt = [0u8; 32];
            getrandom::getrandom(&mut salt).context("generating a salt")?;
            StateFile {
                salt: hex(&salt),
                pushed: BTreeMap::new(),
            }
        };
        Ok(State { path, file })
    }

    /// Salted SHA-256 of `value`, hex-encoded.
    pub fn digest(&self, value: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.file.salt.as_bytes());
        hasher.update(value);
        hex(&hasher.finalize())
    }

    /// The digest last recorded for `key`, if any.
    pub fn last_pushed(&self, key: &str) -> Option<&str> {
        self.file.pushed.get(key).map(String::as_str)
    }

    /// Note that `digest` was pushed to `key`. Takes effect on disk at `save`.
    pub fn record(&mut self, key: String, digest: String) {
        self.file.pushed.insert(key, digest);
    }

    /// Write the state, readable only by the owner.
    pub fn save(&self) -> Result<()> {
        let text = toml::to_string(&self.file).context("serialising state")?;
        repo::write_private(&self.path, text.as_bytes())
    }
}

/// Lowercase hex encoding.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(dir.path()).unwrap();
        let digest = state.digest(b"value");
        assert_eq!(digest.len(), 64);
        assert_ne!(digest, state.digest(b"other"));
        state.record("github:o/r#env/PRODUCTION/X".into(), digest.clone());
        state.save().unwrap();

        let again = State::load(dir.path()).unwrap();
        assert_eq!(
            again.last_pushed("github:o/r#env/PRODUCTION/X"),
            Some(digest.as_str())
        );
        assert_eq!(again.digest(b"value"), digest);
    }
}
