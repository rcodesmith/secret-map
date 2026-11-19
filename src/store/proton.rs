//! Proton Pass through Proton's `pass-cli`.
//!
//! With `auth = "pat-prompt"` (the default) each run prompts for a personal
//! access token, logs in to a session in a fresh temporary directory, fetches,
//! then logs out and deletes the directory. No session outlives the run, so
//! nothing else on the machine can use one between runs. Scope the token to a
//! dedicated vault with `viewer` access.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{bail, Context, Result};
use tempfile::TempDir;
use zeroize::Zeroizing;

use super::{run, run_value, Store};
use crate::map::ProtonAuth;

/// The `proton-pass` backend.
pub struct ProtonPass {
    /// The store's name in the map, used in prompts and messages.
    scheme: String,
    /// The `pass-cli` binary to run.
    command: String,
    auth: ProtonAuth,
}

impl ProtonPass {
    /// `command` defaults to `pass-cli` on the `PATH`.
    pub fn new(scheme: &str, command: Option<String>, auth: ProtonAuth) -> ProtonPass {
        ProtonPass {
            scheme: scheme.to_string(),
            command: command.unwrap_or_else(|| "pass-cli".to_string()),
            auth,
        }
    }
}

/// What the signal handler needs to end the throwaway session, since `Drop`
/// doesn't run when a signal ends the process. Stores fetch one at a time, so
/// there's at most one. Holding the lock means cleanup is under way.
struct LiveSession {
    command: String,
    dir: PathBuf,
}

/// The throwaway session currently open, if any.
static LIVE: Mutex<Option<LiveSession>> = Mutex::new(None);

/// Lock `LIVE`. A panic while it was held doesn't stop cleanup.
fn live() -> MutexGuard<'static, Option<LiveSession>> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Log the session in `dir` out, ignoring any failure: this is best-effort
/// cleanup, and the directory is deleted afterwards either way.
fn logout(command: &str, dir: &Path) {
    let _ = Command::new(command)
        .env("PROTON_PASS_SESSION_DIR", dir)
        .args(["logout", "--force"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Log out and delete the live throwaway session, if there is one.
fn end_live_session() {
    if let Some(session) = live().take() {
        logout(&session.command, &session.dir);
        let _ = fs::remove_dir_all(&session.dir);
    }
}

/// Ctrl-C / termination handler: clean up the session, then exit with 130.
fn exit_on_signal() {
    // If normal cleanup is under way, this waits for it to finish.
    end_live_session();
    std::process::exit(130);
}

/// A pass-cli session. A throwaway session (`dir` set) is logged out on drop,
/// and its directory deleted. A signal gets the same through `exit_on_signal`.
struct Session {
    command: String,
    dir: Option<TempDir>,
}

impl Session {
    /// A `pass-cli` command bound to this session.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.command);
        if let Some(dir) = &self.dir {
            command.env("PROTON_PASS_SESSION_DIR", dir.path());
        }
        command
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            // Held until the directory is gone, so a signal can't exit midway.
            let mut live = live();
            live.take();
            logout(&self.command, dir.path());
            let _ = dir.close();
        }
    }
}

impl ProtonPass {
    /// Start a session per `auth`. For `pat-prompt`, prompt for a token
    /// (mentioning `count`, the number of values to fetch) and log in to a
    /// fresh private directory, registered for cleanup first.
    fn open(&self, count: usize) -> Result<Session> {
        match self.auth {
            ProtonAuth::ExistingSession => Ok(Session {
                command: self.command.clone(),
                dir: None,
            }),
            ProtonAuth::PatPrompt => {
                let token = Zeroizing::new(
                    rpassword::prompt_password(format!(
                        "Proton Pass access token for {}:// ({count} to fetch): ",
                        self.scheme
                    ))
                    .context("reading the access token")?,
                );
                let token = token.trim();
                if token.is_empty() {
                    bail!("no access token entered");
                }
                let dir = tempfile::Builder::new()
                    .prefix("secretmap-proton-")
                    .tempdir()
                    .context("creating a session directory")?;
                // Session material goes here: keep it from other users.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                        .context("restricting the session directory")?;
                }
                // A second Proton store in the same run reuses the handler.
                match ctrlc::set_handler(exit_on_signal) {
                    Ok(()) | Err(ctrlc::Error::MultipleHandlers) => {}
                    Err(err) => return Err(err).context("setting up session cleanup"),
                }
                // Recorded before login, so an interrupted login is cleaned up too.
                *live() = Some(LiveSession {
                    command: self.command.clone(),
                    dir: dir.path().to_path_buf(),
                });
                let session = Session {
                    command: self.command.clone(),
                    dir: Some(dir),
                };
                let mut login = session.command();
                login
                    .arg("login")
                    .env("PROTON_PASS_PERSONAL_ACCESS_TOKEN", token);
                run(&mut login, &format!("{} login", self.command))?;
                Ok(session)
            }
        }
    }
}

impl Store for ProtonPass {
    /// Open one session and read every path with `pass-cli item view`.
    fn fetch(&mut self, paths: &[String]) -> Result<BTreeMap<String, Zeroizing<String>>> {
        let session = self.open(paths.len())?;
        let mut values = BTreeMap::new();
        for path in paths {
            // pass-cli's own URI scheme is always pass://, whatever the map calls the store.
            let uri = format!("pass://{path}");
            let mut view = session.command();
            view.args(["item", "view", &uri]);
            let value = run_value(
                &mut view,
                &format!("{} item view {}://{path}", self.command, self.scheme),
            )?;
            values.insert(path.clone(), value);
        }
        Ok(values)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_signal_logs_out_and_deletes_the_session() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("log");
        let script = root.path().join("fake-pass-cli");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$1 $PROTON_PASS_SESSION_DIR\" >> {}\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let dir = root.path().join("session");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("session.json"), "{}").unwrap();

        *live() = Some(LiveSession {
            command: script.display().to_string(),
            dir: dir.clone(),
        });
        end_live_session();

        assert!(!dir.exists());
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            format!("logout {}\n", dir.display())
        );
        // A second signal has nothing left to do.
        end_live_session();
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    }
}
