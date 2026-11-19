//! Secret stores. A store resolves the part of a reference after `scheme://`.
//!
//! To add a backend: implement [`Store`] in a new module, add a variant to
//! [`crate::map::StoreConfig`], and construct it in [`build`].

mod exec;
mod proton;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use zeroize::Zeroizing;

use crate::map::StoreConfig;

/// The store names a map's templates are checked against.
pub struct Schemes {
    /// Stores the map declares. References to these are fetched.
    pub declared: BTreeSet<String>,
    /// Stores that exist here but the map doesn't declare. A reference to one
    /// is a mistake, not a literal.
    pub undeclared: BTreeSet<String>,
}

/// Whether `name` has the form of a store name.
pub fn is_scheme_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `store://path`, where `store` is a store the map declares it needs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reference {
    /// The scheme: the store's name in the map.
    pub store: String,
    /// Everything after `://`, passed to the store.
    pub path: String,
}

impl Reference {
    /// `Ok(None)` when `value` isn't a reference to one of `schemes`.
    pub fn parse(value: &str, schemes: &BTreeSet<String>) -> Result<Option<Reference>> {
        let Some((scheme, path)) = value.split_once("://") else {
            return Ok(None);
        };
        if !schemes.contains(scheme) {
            return Ok(None);
        }
        // Segments are passed to store CLIs as arguments, so none may look like
        // a flag, or climb out of a directory the store's arguments point at.
        if path.is_empty()
            || path.split('/').any(|segment| {
                let trimmed = segment.trim();
                trimmed.is_empty() || trimmed == "." || trimmed == ".." || segment.starts_with('-')
            })
            || path.chars().any(char::is_control)
        {
            bail!(
                "malformed reference {value}: expected {scheme}://segment/segment, with no \
                 empty, '.' or '..' segment and none starting with '-'"
            );
        }
        Ok(Some(Reference {
            store: scheme.to_string(),
            path: path.to_string(),
        }))
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.store, self.path)
    }
}

/// A secret backend.
pub trait Store {
    /// Fetch every path in one session. Implementations must never print values.
    fn fetch(&mut self, paths: &[String]) -> Result<BTreeMap<String, Zeroizing<String>>>;
}

/// The backend for `config`, serving references with the `scheme` prefix.
pub fn build(scheme: &str, config: &StoreConfig) -> Box<dyn Store> {
    match config {
        StoreConfig::ProtonPass { command, auth } => {
            Box::new(proton::ProtonPass::new(scheme, command.clone(), *auth))
        }
        StoreConfig::Exec { command, args } => {
            Box::new(exec::Exec::new(command.clone(), args.clone()))
        }
    }
}

/// Run a command and fail unless it exits successfully.
fn run(command: &mut Command, what: &str) -> Result<()> {
    let out = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!(
            "{what} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Run a command and return its stdout, minus one trailing newline, as a secret.
fn run_value(command: &mut Command, what: &str) -> Result<Zeroizing<String>> {
    let out = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("running {what}"))?;
    let stdout = Zeroizing::new(out.stdout);
    if !out.status.success() {
        bail!(
            "{what} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text =
        std::str::from_utf8(&stdout).with_context(|| format!("{what} printed non-UTF-8 output"))?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    if text.is_empty() {
        bail!("{what} returned an empty value");
    }
    Ok(Zeroizing::new(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schemes() -> BTreeSet<String> {
        ["pass".to_string()].into()
    }

    #[test]
    fn parses_references() {
        let r = Reference::parse("pass://Vault/item/field", &schemes())
            .unwrap()
            .unwrap();
        assert_eq!(r.store, "pass");
        assert_eq!(r.path, "Vault/item/field");
        assert_eq!(r.to_string(), "pass://Vault/item/field");
        assert!(
            Reference::parse("pass://Vault Name/item/TOTP 1", &schemes())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn non_references() {
        assert!(Reference::parse("https://x/y", &schemes())
            .unwrap()
            .is_none());
        assert!(Reference::parse("plain", &schemes()).unwrap().is_none());
    }

    #[test]
    fn malformed_references() {
        for value in [
            "pass://",
            "pass://a//b",
            "pass://-flag/x",
            "pass://a/ /b",
            "pass://a\n",
            "pass://..",
            "pass://.",
            "pass://../../etc/passwd",
            "pass://a/../b",
            "pass://a/./b",
            "pass://a/ .. /b",
        ] {
            assert!(Reference::parse(value, &schemes()).is_err(), "{value:?}");
        }
    }

    #[test]
    fn dots_inside_a_segment_are_allowed() {
        for value in [
            "pass://a.b/c",
            "pass://.hidden/x",
            "pass://a/..b",
            "pass://a/b...",
        ] {
            assert!(
                Reference::parse(value, &schemes()).unwrap().is_some(),
                "{value:?}"
            );
        }
    }
}
