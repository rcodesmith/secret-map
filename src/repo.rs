//! Git and filesystem helpers. Every path the map names is checked to stay
//! inside the repository and off symlinks.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// Run git (in `root`, if given) and return its stdout, trimmed. Fails
/// with git's stderr if it exits unsuccessfully.
fn git(root: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    if let Some(root) = root {
        command.arg("-C").arg(root);
    }
    let out = command
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)
        .context("git printed non-UTF-8 output")?
        .trim_end()
        .to_string())
}

/// The top-level directory of the current git repository.
pub fn root() -> Result<PathBuf> {
    git(None, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .context("secretmap runs inside a git repository")
}

/// The absolute path of the repository's git directory (usually `.git`).
pub fn git_dir(root: &Path) -> Result<PathBuf> {
    git(Some(root), &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

/// `root/rel`, provided `rel` is relative, has no `..`, doesn't start with
/// `:`, doesn't go through `.git`, and no existing part of it is a symlink.
///
/// Git reads a leading `:` as pathspec magic, so `:/.env` would be checked as
/// `.env` while being written to a different, unignored path.
pub fn resolve_in_repo(root: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.as_os_str().is_empty() {
        bail!("empty path");
    }
    let first = rel.components().find(|c| *c != Component::CurDir);
    if let Some(Component::Normal(first)) = first {
        if first.to_string_lossy().starts_with(':') {
            bail!(
                "{} must not start with ':', which git reads as pathspec magic",
                rel.display()
            );
        }
    }
    let mut current = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                // Case-insensitive filesystems treat `.GIT` as the same directory.
                if part.eq_ignore_ascii_case(".git") {
                    bail!("{} must not be inside .git", rel.display());
                }
                current.push(part);
                if let Ok(meta) = fs::symlink_metadata(&current) {
                    if meta.file_type().is_symlink() {
                        bail!(
                            "{} goes through a symlink ({})",
                            rel.display(),
                            current.display()
                        );
                    }
                }
            }
            Component::CurDir => {}
            _ => bail!(
                "{} must be a relative path inside the repository, without '..'",
                rel.display()
            ),
        }
    }
    Ok(current)
}

/// Whether git ignores `rel`. Tracked files count as not ignored.
pub fn is_ignored(root: &Path, rel: &Path) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", "--"])
        .arg(rel)
        .stdin(Stdio::null())
        .status()
        .context("running git check-ignore")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git check-ignore failed for {}", rel.display()),
    }
}

/// Whether `rel` sits in a directory git ignores. Such directories are often
/// build output that gets published, so an ignored file there isn't private.
pub fn in_ignored_dir(root: &Path, rel: &Path) -> Result<bool> {
    let Some(parent) = rel.parent() else {
        return Ok(false);
    };
    let parent: PathBuf = parent
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    if parent.as_os_str().is_empty() {
        return Ok(false);
    }
    // The slash makes `dist/` rules match even if the directory doesn't exist yet.
    let mut dir = parent.into_os_string();
    dir.push("/");
    is_ignored(root, Path::new(&dir))
}

/// Whether git tracks `rel` (it's in the index).
pub fn is_tracked(root: &Path, rel: &Path) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        // Literal, or `a[b]` would match a tracked `ab` while `a[b]` is what's read.
        .args(["--literal-pathspecs", "ls-files", "--error-unmatch", "--"])
        .arg(rel)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running git ls-files")?;
    Ok(status.success())
}

/// Whether `rel` exists in `HEAD` and the working copy has the same content.
pub fn matches_head(root: &Path, rel: &Path) -> Result<bool> {
    let rel: PathBuf = rel
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    // Checked first: `git diff` reports no change for an untracked file.
    let mut object = std::ffi::OsString::from("HEAD:");
    object.push(&rel);
    let in_head = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "-e"])
        .arg(object)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running git cat-file")?;
    if !in_head.success() {
        return Ok(false);
    }
    let diff = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "--literal-pathspecs",
            "diff",
            "--quiet",
            "--no-ext-diff",
            "HEAD",
            "--",
        ])
        .arg(&rel)
        .stdin(Stdio::null())
        .status()
        .context("running git diff")?;
    match diff.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git diff failed for {}", rel.display()),
    }
}

/// The URL of the `origin` remote. It can hold a token, so callers must keep
/// it out of messages.
pub fn origin_url(root: &Path) -> Result<String> {
    git(Some(root), &["remote", "get-url", "origin"])
}

/// A private directory in `dir` that git ignores whatever the repository's
/// rules say, so a file left there by a killed process can't be committed.
/// Being inside `dir`, it is on the same filesystem, so renames out are atomic.
fn staging_dir(dir: &Path) -> Result<tempfile::TempDir> {
    let staging = tempfile::Builder::new()
        .prefix(".secretmap-")
        .tempdir_in(dir)
        .with_context(|| format!("creating a temporary directory in {}", dir.display()))?;
    fs::write(staging.path().join(".gitignore"), "*\n")
        .with_context(|| format!("writing a .gitignore in {}", staging.path().display()))?;
    Ok(staging)
}

/// Write atomically, readable only by the owner.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let staging = staging_dir(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(staging.path())
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(contents)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotfiles_are_valid_paths() {
        let root = tempfile::tempdir().unwrap();
        for path in [".env", "./.env", ".config/app.env", "config/.env", "a/:b"] {
            assert!(
                resolve_in_repo(root.path(), Path::new(path)).is_ok(),
                "{path}"
            );
        }
    }

    #[test]
    fn pathspec_magic_is_refused() {
        let root = tempfile::tempdir().unwrap();
        for path in [":/.env", ":(top).env", ":!x", ":^x", "./:/.env", ":x"] {
            let err = resolve_in_repo(root.path(), Path::new(path)).unwrap_err();
            assert!(err.to_string().contains("':'"), "{path}: {err}");
        }
    }

    #[test]
    fn staged_secrets_are_invisible_to_git() {
        let root = tempfile::tempdir().unwrap();
        git(Some(root.path()), &["init", "-q"]).unwrap();
        // No ignore rule covers the staging directory: it has to cover itself.
        let staging = staging_dir(root.path()).unwrap();
        fs::write(staging.path().join("secret"), "A=1\n").unwrap();
        let rel = staging.path().strip_prefix(root.path()).unwrap();
        assert!(is_ignored(root.path(), &rel.join("secret")).unwrap());
        git(Some(root.path()), &["add", "-A"]).unwrap();
        assert_eq!(git(Some(root.path()), &["ls-files"]).unwrap(), "");
        let status = git(
            Some(root.path()),
            &["status", "--porcelain", "--untracked-files=all"],
        );
        assert_eq!(status.unwrap(), "");
    }

    #[test]
    fn writing_leaves_only_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        write_private(&dir.path().join(".env"), b"A=1\n").unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [".env"]);
    }

    #[test]
    fn paths_stay_inside() {
        let root = tempfile::tempdir().unwrap();
        assert!(resolve_in_repo(root.path(), Path::new(".env")).is_ok());
        assert!(resolve_in_repo(root.path(), Path::new("./config/a")).is_ok());
        assert!(resolve_in_repo(root.path(), Path::new("../x")).is_err());
        assert!(resolve_in_repo(root.path(), Path::new("a/../../x")).is_err());
        assert!(resolve_in_repo(root.path(), Path::new("/etc/passwd")).is_err());
        assert!(resolve_in_repo(root.path(), Path::new("")).is_err());
    }

    #[test]
    fn git_directories_are_refused() {
        let root = tempfile::tempdir().unwrap();
        for path in [".git/config", "./.git/hooks/x", "sub/.git/x", ".GIT/config"] {
            let err = resolve_in_repo(root.path(), Path::new(path)).unwrap_err();
            assert!(err.to_string().contains(".git"), "{path}: {err}");
        }
        assert!(resolve_in_repo(root.path(), Path::new(".github/x.env")).is_ok());
    }

    #[test]
    fn ignored_directories_are_detected() {
        let root = tempfile::tempdir().unwrap();
        git(Some(root.path()), &["init", "-q"]).unwrap();
        fs::write(root.path().join(".gitignore"), "dist/\nout/*\n.env\n").unwrap();
        for path in ["dist/app.env", "dist/sub/.env", "./dist/.env", "out/.env"] {
            assert!(
                in_ignored_dir(root.path(), Path::new(path)).unwrap(),
                "{path}"
            );
        }
        for path in [".env", "./.env", "config/.env"] {
            assert!(
                !in_ignored_dir(root.path(), Path::new(path)).unwrap(),
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/tmp", root.path().join("link")).unwrap();
        assert!(resolve_in_repo(root.path(), Path::new("link/.env")).is_err());
    }

    #[test]
    fn writes_private_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/.env");
        write_private(&path, b"A=1\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=1\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
