//! A store backed by any CLI that prints one secret to stdout, configured
//! in the user's `~/.config/secretmap/config.toml`. For example, the local
//! `pass-cli` from github.com/reyamira/pass-cli:
//!
//! ```toml
//! [stores.lp]
//! backend = "exec"
//! command = "/usr/local/bin/pass-cli"
//! args = ["get", "{0}", "--quiet", "--field", "{1}"]   # lp://github/password
//! ```

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use zeroize::Zeroizing;

use super::{run_value, Store};

/// The `exec` backend: runs `command` once per reference.
pub struct Exec {
    command: String,
    /// Argument templates; see `expand`.
    args: Vec<String>,
}

impl Exec {
    pub fn new(command: String, args: Vec<String>) -> Exec {
        Exec { command, args }
    }
}

impl Store for Exec {
    /// Run the command for each path in turn; the first failure stops the fetch.
    fn fetch(&mut self, paths: &[String]) -> Result<BTreeMap<String, Zeroizing<String>>> {
        let mut values = BTreeMap::new();
        for path in paths {
            let segments: Vec<&str> = path.split('/').collect();
            let args = self
                .args
                .iter()
                .map(|arg| expand(arg, path, &segments))
                .collect::<Result<Vec<_>>>()?;
            let mut command = Command::new(&self.command);
            command.args(&args);
            let value = run_value(&mut command, &format!("{} for {path}", self.command))?;
            values.insert(path.clone(), value);
        }
        Ok(values)
    }
}

/// Replace `{path}` and `{N}` in one argument.
fn expand(arg: &str, path: &str, segments: &[&str]) -> Result<String> {
    let mut out = String::new();
    let mut rest = arg;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after
            .find('}')
            .with_context(|| format!("unclosed '{{' in exec argument {arg:?}"))?;
        let name = &after[..end];
        if name == "path" {
            out.push_str(path);
        } else {
            let index: usize = name.parse().map_err(|_| {
                anyhow!("unknown placeholder {{{name}}} in exec argument {arg:?}; use {{path}}, {{0}}, {{1}}, ...")
            })?;
            let segment = segments
                .get(index)
                .with_context(|| format!("reference {path} has no segment {index}"))?;
            out.push_str(segment);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_placeholders() {
        let segs = ["github", "password"];
        assert_eq!(expand("{0}", "github/password", &segs).unwrap(), "github");
        assert_eq!(
            expand("--field={1}", "github/password", &segs).unwrap(),
            "--field=password"
        );
        assert_eq!(
            expand("x{path}y", "github/password", &segs).unwrap(),
            "xgithub/passwordy"
        );
        assert_eq!(expand("plain", "a", &["a"]).unwrap(), "plain");
    }

    #[test]
    fn rejects_bad_placeholders() {
        assert!(expand("{2}", "a/b", &["a", "b"]).is_err());
        assert!(expand("{name}", "a", &["a"]).is_err());
        assert!(expand("{0", "a", &["a"]).is_err());
    }
}
