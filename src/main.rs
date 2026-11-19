//! secretmap: render a project's config from a committed map and templates,
//! pulling secret values from a pluggable store, and deliver it to local env
//! files (`pull`) or remote destinations such as GitHub secrets (`push`).

mod dest;
mod map;
mod repo;
mod store;
mod template;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use zeroize::Zeroizing;

use map::{Dest, Map, Source, Target};
use store::Reference;

/// Command-line arguments.
#[derive(Parser)]
#[command(name = "secretmap", version, about)]
struct Cli {
    /// The map file. Default: secretmap.toml at the root of the current git repository.
    #[arg(long, global = true, value_name = "PATH")]
    map: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// What to do with the map.
#[derive(Subcommand)]
enum Command {
    /// Validate the map and its templates. Reads no secrets.
    Check,
    /// List the map's targets.
    List,
    /// Write the map's file targets, fetching secrets from the store.
    Pull(RunArgs),
    /// Send the map's remote targets (e.g. GitHub secrets), fetching secrets from the store.
    Push(RunArgs),
}

/// Options shared by `pull` and `push`.
#[derive(clap::Args)]
struct RunArgs {
    /// Only this target (repeatable). Default: every target of this kind.
    #[arg(long = "target", value_name = "NAME")]
    targets: Vec<String>,

    /// Report what would change, by key name only. Still reads the store.
    #[arg(long)]
    dry_run: bool,

    /// pull: replace files secretmap didn't generate. push: push values that look unchanged.
    #[arg(long)]
    force: bool,
}

/// Run, printing any error with its context chain and exiting with status 1.
fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

/// Find the repository, load and validate the map, print its warnings, and
/// dispatch to the subcommand.
fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = repo::root()?;
    let map_path = cli
        .map
        .clone()
        .unwrap_or_else(|| root.join("secretmap.toml"));
    let map = Map::load(&root, &map_path)?;
    for warning in map.warnings() {
        eprintln!("warning: {warning}");
    }

    match cli.command {
        Command::Check => check(&root, &map),
        Command::List => {
            list(&map);
            Ok(())
        }
        Command::Pull(args) => pull(&root, &map, &args),
        Command::Push(args) => push(&root, &map, &args),
    }
}

/// `check`: report each target, the remote destinations they resolve to,
/// and which targets use each store. Fetches nothing.
fn check(root: &Path, map: &Map) -> Result<()> {
    // Map::load has already validated paths, templates and references.
    for line in
        dest::Remotes::resolve(root, map.targets.iter().filter_map(Target::remote))?.describe()
    {
        println!("{line}");
    }
    let mut per_store: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for target in &map.targets {
        for reference in target.source.references() {
            per_store
                .entry(reference.store)
                .or_default()
                .insert(target.name.as_str());
        }
    }
    for target in &map.targets {
        println!("ok  {}", describe(target));
    }
    for (scheme, targets) in per_store {
        let backend = map.stores.get(&scheme).map_or("?", |s| s.backend_name());
        println!("store {scheme}:// ({backend}): used by {}", join(targets));
    }
    Ok(())
}

/// `list`: one line per target.
fn list(map: &Map) {
    for target in &map.targets {
        println!("{}", describe(target));
    }
}

/// One line for a target: its name, source and destination.
fn describe(target: &Target) -> String {
    let source = match &target.source {
        Source::Template { path, template } => {
            format!("{} ({} keys)", path.display(), template.keys().len())
        }
        Source::Value(reference) => reference.to_string(),
    };
    let dest = match &target.dest {
        Dest::File(path) => path.display().to_string(),
        Dest::Remote(remote) => remote.label(),
    };
    format!("{:<20} {source} -> {dest}", target.name)
}

/// `pull`: render the selected file targets and write them. Without
/// `--force`, refuses to replace a file that lacks secretmap's header.
fn pull(root: &Path, map: &Map, args: &RunArgs) -> Result<()> {
    let targets = select(map, &args.targets, true)?;
    if targets.is_empty() {
        println!("No file targets in the map.");
        return Ok(());
    }

    // Refuse before asking for any credential.
    if !args.force {
        for target in &targets {
            let rel = target.file().expect("selected file targets");
            let path = root.join(rel);
            if path.exists() {
                let existing = Zeroizing::new(
                    fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", rel.display()))?,
                );
                if !template::has_header(&existing) {
                    bail!(
                        "{} exists and secretmap didn't generate it. Compare it with the \
                         template (secretmap pull --dry-run --force --target {}), then re-run \
                         with --force to replace it",
                        rel.display(),
                        target.name
                    );
                }
            }
        }
    }

    let secrets = fetch(map, &targets)?;
    // Render everything first, so a bad value leaves every file untouched.
    let rendered = targets
        .into_iter()
        .map(|target| Ok((target, render(target, &secrets)?)))
        .collect::<Result<Vec<_>>>()?;
    for (target, body) in rendered {
        let rel = target.file().expect("selected file targets");
        let path = root.join(rel);
        if args.dry_run {
            print_diff(rel, &path, &body)?;
            continue;
        }
        repo::write_private(&path, body.as_bytes())?;
        println!(
            "wrote {} ({} keys)",
            rel.display(),
            template::env_entries(&body).len()
        );
    }
    Ok(())
}

/// `push`: render the selected remote targets and send each one whose value
/// changed since the last push from this clone (or every one, with `--force`).
/// Templates must match `HEAD`, so only committed config is pushed.
fn push(root: &Path, map: &Map, args: &RunArgs) -> Result<()> {
    let targets = select(map, &args.targets, false)?;
    if targets.is_empty() {
        println!("No remote targets in the map.");
        return Ok(());
    }
    let remotes = dest::Remotes::resolve(root, targets.iter().filter_map(|t| t.remote()))?;
    for line in remotes.describe() {
        println!("{line}");
    }

    // Refuse before asking for any credential. `pull` has no such rule, so a
    // template can be edited and tried locally before it's committed.
    for target in &targets {
        if let Source::Template { path, .. } = &target.source {
            if !repo::matches_head(root, path)? {
                bail!(
                    "template {} has uncommitted changes; commit it before pushing",
                    path.display()
                );
            }
        }
    }

    let secrets = fetch(map, &targets)?;
    // Render everything first, so a bad value leaves every secret untouched.
    let rendered = targets
        .into_iter()
        .map(|target| {
            let remote = target.remote().expect("selected remote targets");
            Ok((remote, render(target, &secrets)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut state = dest::State::load(&repo::git_dir(root)?)?;
    for (remote, body) in rendered {
        let key = remotes.state_key(remote);
        let digest = state.digest(body.as_bytes());
        let label = remote.label();
        if !args.force && state.last_pushed(&key) == Some(digest.as_str()) {
            println!(
                "{label}: unchanged since the last push from here, skipped (--force pushes anyway)"
            );
            continue;
        }
        if args.dry_run {
            println!("{label}: would push");
            continue;
        }
        remotes.set(remote, body.as_bytes())?;
        state.record(key, digest);
        state.save()?;
        println!("{label}: pushed");
    }
    Ok(())
}

/// The named targets (or all) whose destination is a file (`files`) or remote.
fn select<'a>(map: &'a Map, names: &[String], files: bool) -> Result<Vec<&'a Target>> {
    let kind = if files { "file" } else { "remote" };
    for name in names {
        let target = map
            .targets
            .iter()
            .find(|t| &t.name == name)
            .with_context(|| format!("no target named {name}"))?;
        if target.is_file() != files {
            bail!("target {name} isn't a {kind} target");
        }
    }
    Ok(map
        .targets
        .iter()
        .filter(|t| t.is_file() == files && (names.is_empty() || names.contains(&t.name)))
        .collect())
}

/// Fetch every reference the targets use, one store session per store.
fn fetch(map: &Map, targets: &[&Target]) -> Result<BTreeMap<Reference, Zeroizing<String>>> {
    let mut by_store: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for target in targets {
        for reference in target.source.references() {
            by_store
                .entry(reference.store.clone())
                .or_default()
                .insert(reference.path.clone());
        }
    }

    let mut values = BTreeMap::new();
    for (scheme, paths) in by_store {
        let config = map
            .stores
            .get(&scheme)
            .with_context(|| format!("no store named {scheme}"))?;
        let mut store = store::build(&scheme, config);
        let paths: Vec<String> = paths.into_iter().collect();
        let fetched = store
            .fetch(&paths)
            .with_context(|| format!("fetching from store {scheme}"))?;
        for path in paths {
            let value = fetched
                .get(&path)
                .with_context(|| format!("store {scheme} returned nothing for {path}"))?;
            values.insert(
                Reference {
                    store: scheme.clone(),
                    path,
                },
                value.clone(),
            );
        }
    }
    Ok(values)
}

/// A target's final value: a rendered template (with the generated-file
/// header if it goes to a file), or the single fetched value.
fn render(
    target: &Target,
    secrets: &BTreeMap<Reference, Zeroizing<String>>,
) -> Result<Zeroizing<String>> {
    match &target.source {
        Source::Template { path, template } => {
            let body = template.render(secrets)?;
            if target.is_file() {
                let mut out = Zeroizing::new(template::header(path, &target.name));
                out.push_str(&body);
                Ok(out)
            } else {
                Ok(body)
            }
        }
        Source::Value(reference) => secrets
            .get(reference)
            .cloned()
            .with_context(|| format!("no value fetched for {reference}")),
    }
}

/// For `pull --dry-run`: which keys a write would add, remove or change.
/// Never prints values, nor names of keys only the existing file has.
fn print_diff(rel: &Path, path: &Path, body: &str) -> Result<()> {
    let existing = if path.exists() {
        Some(Zeroizing::new(
            fs::read_to_string(path).with_context(|| format!("reading {}", rel.display()))?,
        ))
    } else {
        None
    };
    if existing.as_ref().is_some_and(|text| text.as_str() == body) {
        println!("{}: no changes", rel.display());
        return Ok(());
    }

    let new = template::env_entries(body);
    let old = existing
        .as_ref()
        .map(|text| template::env_entries(text))
        .unwrap_or_default();
    let added: Vec<&str> = new
        .keys()
        .filter(|k| !old.contains_key(*k))
        .map(String::as_str)
        .collect();
    // Only counted: text in the existing file can be anything, including part
    // of a secret, so names are printed only if the template has them too.
    let removed = old.keys().filter(|k| !new.contains_key(*k)).count();
    let changed: Vec<&str> = new
        .iter()
        .filter(|(k, v)| old.get(*k).is_some_and(|o| o.as_str() != v.as_str()))
        .map(|(k, _)| k.as_str())
        .collect();

    if added.is_empty() && removed == 0 && changed.is_empty() {
        let note = if existing.is_some() {
            "formatting or comments differ"
        } else {
            "new file with no keys"
        };
        println!("{}: {note}", rel.display());
        return Ok(());
    }
    println!("{}:", rel.display());
    if !added.is_empty() {
        println!("  added:   {}", join(added));
    }
    if removed > 0 {
        let entries = if removed == 1 { "entry" } else { "entries" };
        println!("  removed: {removed} existing {entries} (names not shown)");
    }
    if !changed.is_empty() {
        println!("  changed: {}", join(changed));
    }
    Ok(())
}

/// Comma-separated list.
fn join<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    items.into_iter().collect::<Vec<_>>().join(", ")
}
