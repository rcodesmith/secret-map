//! The committed map: which stores a project needs, and which targets to produce.
//!
//! The map is treated as untrusted input: a malicious commit must not be able
//! to make secretmap write secrets outside the repository or into a tracked
//! file, or send them anywhere but the destinations the repository itself
//! implies (for GitHub, the `origin` repository). Nor may it choose what gets executed,
//! so how a store is reached (its command, its authentication) is defined only
//! in the user's own config file, never in the map.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::dest::{GithubSecret, Remote};
use crate::repo;
use crate::store::{is_scheme_name, Reference, Schemes};
use crate::template::Template;

/// The store available without any user configuration.
const BUILTIN_STORE: &str = "pass";

/// URL schemes that commonly appear in plain config values; a store with one
/// of these names would turn ordinary URLs into secret lookups.
const RESERVED_SCHEMES: &[&str] = &[
    "http",
    "https",
    "ws",
    "wss",
    "file",
    "ftp",
    "ssh",
    "git",
    "postgres",
    "postgresql",
    "mysql",
    "redis",
    "rediss",
    "amqp",
    "mongodb",
    "s3",
    "gs",
];

/// `secretmap.toml` as written, before validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMap {
    /// Map format version. Only 1 is supported.
    version: u32,
    /// The reference schemes this project uses.
    #[serde(default = "default_stores")]
    stores: Vec<String>,
    #[serde(default, rename = "target")]
    targets: Vec<RawTarget>,
}

/// One `[[target]]` as written. Exactly one source (`template` or `value`)
/// and exactly one destination must be set; `build_target` checks that.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    name: String,
    /// A committed env-file template, relative to the repository root.
    template: Option<PathBuf>,
    /// A single reference, such as `pass://vault/item/field`.
    value: Option<String>,
    // Destinations: a local file, or one of the remote kinds in `dest`.
    file: Option<PathBuf>,
    github: Option<GithubSecret>,
}

/// Stores a map uses when it doesn't list any: just the built-in one.
fn default_stores() -> Vec<String> {
    vec![BUILTIN_STORE.to_string()]
}

/// `~/.config/secretmap/config.toml`: the user's own store definitions.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct UserConfig {
    #[serde(default)]
    stores: BTreeMap<String, StoreConfig>,
}

/// A store definition, from the user's config. The key it's declared under is
/// the reference scheme: `[stores.pass]` makes `pass://...` resolve through it.
#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "backend", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StoreConfig {
    /// Proton Pass, via Proton's `pass-cli`.
    ProtonPass {
        /// The binary to run. Default: `pass-cli`.
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        auth: ProtonAuth,
    },
    /// Any CLI that prints one secret to stdout. `args` may use `{path}`
    /// (everything after `scheme://`) and `{0}`, `{1}`, ... (its `/`-separated segments).
    Exec { command: String, args: Vec<String> },
}

impl StoreConfig {
    /// The `backend` value this was declared with, for display.
    pub fn backend_name(&self) -> &'static str {
        match self {
            StoreConfig::ProtonPass { .. } => "proton-pass",
            StoreConfig::Exec { .. } => "exec",
        }
    }
}

/// How the Proton Pass backend authenticates.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProtonAuth {
    /// Prompt for a personal access token, log in to a throwaway session,
    /// fetch, log out. Nothing persists between runs.
    #[default]
    PatPrompt,
    /// Use whatever session pass-cli already has.
    ExistingSession,
}

/// A validated map: every path, template, reference and destination in it
/// has been checked.
pub struct Map {
    /// The stores the map declares, keyed by scheme.
    pub stores: BTreeMap<String, StoreConfig>,
    pub targets: Vec<Target>,
}

/// One thing to produce: a value from `source`, delivered to `dest`.
pub struct Target {
    /// Unique within the map; selects the target with `--target`.
    pub name: String,
    pub source: Source,
    pub dest: Dest,
}

/// Where a target's value comes from.
pub enum Source {
    /// An env-file template, rendered with its references filled in.
    Template { path: PathBuf, template: Template },
    /// One secret, used as is.
    Value(Reference),
}

/// Where a target's value goes.
pub enum Dest {
    /// A local env file, written by `pull`.
    File(PathBuf),
    /// Somewhere `push` sends the value.
    Remote(Remote),
}

impl Target {
    /// Whether this target is written by `pull` (rather than sent by `push`).
    pub fn is_file(&self) -> bool {
        matches!(self.dest, Dest::File(_))
    }

    /// The file path, for a file target.
    pub fn file(&self) -> Option<&Path> {
        match &self.dest {
            Dest::File(path) => Some(path),
            Dest::Remote(_) => None,
        }
    }

    /// The remote destination, for a remote target.
    pub fn remote(&self) -> Option<&Remote> {
        match &self.dest {
            Dest::Remote(remote) => Some(remote),
            Dest::File(_) => None,
        }
    }
}

impl Source {
    /// Every store reference this source needs fetched.
    pub fn references(&self) -> Vec<Reference> {
        match self {
            Source::Template { template, .. } => template.references(),
            Source::Value(reference) => vec![reference.clone()],
        }
    }
}

impl Map {
    /// Read, parse and validate the map at `map_path`, resolving its stores
    /// from the user's config. Targets must have unique names and
    /// destinations.
    pub fn load(root: &Path, map_path: &Path) -> Result<Map> {
        let text = fs::read_to_string(map_path)
            .with_context(|| format!("reading {}", map_path.display()))?;
        let raw: RawMap =
            toml::from_str(&text).with_context(|| format!("parsing {}", map_path.display()))?;
        if raw.version != 1 {
            bail!(
                "{}: unsupported version {} (expected 1)",
                map_path.display(),
                raw.version
            );
        }

        let config_path = user_config_path();
        let defined = load_user_stores(config_path.as_deref())?;
        let mut known: BTreeSet<String> = defined.keys().cloned().collect();
        known.insert(BUILTIN_STORE.to_string());
        let stores = resolve_stores(&raw.stores, defined)?;
        let declared: BTreeSet<String> = stores.keys().cloned().collect();
        let schemes = Schemes {
            undeclared: known.difference(&declared).cloned().collect(),
            declared,
        };

        let mut names = BTreeSet::new();
        let mut dests = BTreeSet::new();
        let mut targets = Vec::new();
        for raw_target in raw.targets {
            let name = raw_target.name.clone();
            let target = build_target(root, &schemes, raw_target)
                .with_context(|| format!("{}: target {name}", map_path.display()))?;
            if !names.insert(target.name.clone()) {
                bail!("{}: two targets are named {name}", map_path.display());
            }
            let dest_key = match &target.dest {
                Dest::File(path) => {
                    let path: PathBuf = path
                        .components()
                        .filter(|c| *c != Component::CurDir)
                        .collect();
                    format!("file:{}", path.display())
                }
                Dest::Remote(remote) => remote.identity(),
            };
            if !dests.insert(dest_key) {
                bail!(
                    "{}: target {name} has the same destination as another",
                    map_path.display()
                );
            }
            targets.push(target);
        }
        Ok(Map { stores, targets })
    }

    /// Template literals that look like references to a store nobody defined.
    /// They might be real URLs, so they're reported rather than refused.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for target in &self.targets {
            let Source::Template { path, template } = &target.source else {
                continue;
            };
            for (key, scheme) in template.literal_schemes() {
                if !RESERVED_SCHEMES.contains(&scheme) {
                    warnings.push(format!(
                        "{}: {key} is copied as written, because {scheme}:// isn't a store \
                         this map declares",
                        path.display()
                    ));
                }
            }
        }
        warnings
    }
}

/// `$XDG_CONFIG_HOME/secretmap/config.toml`, falling back to
/// `~/.config/secretmap/config.toml`. `None` if neither variable is usable.
fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("secretmap").join("config.toml"))
}

/// The stores defined in the user's config; none if it doesn't exist.
fn load_user_stores(path: Option<&Path>) -> Result<BTreeMap<String, StoreConfig>> {
    let Some(path) = path.filter(|path| path.exists()) else {
        return Ok(BTreeMap::new());
    };
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config: UserConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(config.stores)
}

/// The definitions of the stores the map asks for. `pass` is built in, as
/// Proton Pass with a prompted token, unless the user's config redefines it.
fn resolve_stores(
    wanted: &[String],
    mut defined: BTreeMap<String, StoreConfig>,
) -> Result<BTreeMap<String, StoreConfig>> {
    defined
        .entry(BUILTIN_STORE.to_string())
        .or_insert(StoreConfig::ProtonPass {
            command: None,
            auth: ProtonAuth::PatPrompt,
        });
    let mut stores = BTreeMap::new();
    for scheme in wanted {
        validate_scheme(scheme)?;
        let config = defined.get(scheme).with_context(|| {
            format!("the map needs store {scheme}, which isn't defined: add [stores.{scheme}] to ~/.config/secretmap/config.toml")
        })?;
        stores.insert(scheme.clone(), config.clone());
    }
    Ok(stores)
}

/// Validate one raw target. A template must be committed; a file
/// destination must be gitignored, and not by way of an ignored directory.
fn build_target(root: &Path, schemes: &Schemes, raw: RawTarget) -> Result<Target> {
    if raw.name.is_empty()
        || !raw
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("target names may use only letters, digits, '-' and '_'");
    }

    let source = match (raw.template, raw.value) {
        (Some(path), None) => {
            let full = repo::resolve_in_repo(root, &path)?;
            // This stops a map from "templating" an arbitrary local file. Local
            // edits are allowed here; `push` also requires a match with HEAD.
            if !repo::is_tracked(root, &path)? {
                bail!("template {} isn't committed to git", path.display());
            }
            let text =
                fs::read_to_string(&full).with_context(|| format!("reading {}", path.display()))?;
            let template = Template::parse(&text, schemes)
                .with_context(|| format!("template {}", path.display()))?;
            Source::Template { path, template }
        }
        (None, Some(value)) => {
            let reference = Reference::parse(&value, &schemes.declared)?.ok_or_else(|| {
                anyhow!(
                    "value must reference a declared store, for example pass://vault/item/field"
                )
            })?;
            Source::Value(reference)
        }
        _ => bail!("set exactly one of template or value"),
    };

    let remotes: Vec<Remote> = raw.github.into_iter().map(Remote::Github).collect();
    let dest = match (raw.file, remotes.len()) {
        (Some(path), 0) => {
            if matches!(source, Source::Value(_)) {
                bail!("a file target needs a template");
            }
            repo::resolve_in_repo(root, &path)?;
            if !repo::is_ignored(root, &path)? {
                bail!(
                    "{} isn't gitignored (or is tracked); secretmap writes secrets only to ignored files",
                    path.display()
                );
            }
            if repo::in_ignored_dir(root, &path)? {
                bail!(
                    "{} is inside an ignored directory, which may get published; \
                     ignore the file itself rather than its directory",
                    path.display()
                );
            }
            Dest::File(path)
        }
        (None, 1) => {
            let remote = remotes.into_iter().next().expect("one remote");
            remote.validate()?;
            Dest::Remote(remote)
        }
        _ => bail!("set exactly one destination: file or github"),
    };

    Ok(Target {
        name: raw.name,
        source,
        dest,
    })
}

/// A store name must look like a URL scheme and not be a common real one.
fn validate_scheme(scheme: &str) -> Result<()> {
    if !is_scheme_name(scheme) {
        bail!(
            "store name {scheme:?}: use lowercase letters, digits and '-', starting with a letter"
        );
    }
    if RESERVED_SCHEMES.contains(&scheme) {
        bail!("store name {scheme:?} is a common URL scheme; ordinary {scheme}:// values would be treated as secrets");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes() {
        assert!(validate_scheme("pass").is_ok());
        assert!(validate_scheme("proton-2").is_ok());
        assert!(validate_scheme("https").is_err());
        assert!(validate_scheme("Pass").is_err());
        assert!(validate_scheme("2pass").is_err());
        assert!(validate_scheme("").is_err());
    }

    #[test]
    fn store_config_parses() {
        let raw: UserConfig = toml::from_str(
            r#"
            [stores.pass]
            backend = "proton-pass"
            [stores.lp]
            backend = "exec"
            command = "pass-cli"
            args = ["get", "{0}", "--quiet", "--field", "{1}"]
            "#,
        )
        .unwrap();
        assert!(matches!(
            raw.stores["pass"],
            StoreConfig::ProtonPass {
                command: None,
                auth: ProtonAuth::PatPrompt
            }
        ));
        assert_eq!(raw.stores["lp"].backend_name(), "exec");
    }

    #[test]
    fn a_map_cannot_define_a_store() {
        let result: Result<RawMap, _> = toml::from_str(
            r#"
            version = 1
            [stores.pass]
            backend = "exec"
            command = "./evil.sh"
            args = []
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn stores_resolve_from_builtin_and_user_config() {
        let stores = resolve_stores(&default_stores(), BTreeMap::new()).unwrap();
        assert_eq!(stores["pass"].backend_name(), "proton-pass");

        let mut defined = BTreeMap::new();
        defined.insert(
            "pass".to_string(),
            StoreConfig::Exec {
                command: "x".into(),
                args: vec![],
            },
        );
        let stores = resolve_stores(&default_stores(), defined).unwrap();
        assert_eq!(stores["pass"].backend_name(), "exec");

        assert!(resolve_stores(&["other".to_string()], BTreeMap::new()).is_err());
    }
}
