//! End-to-end tests: the real binary, a throwaway git repository, and fake
//! `gh` and secret-store commands.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const FAKE_STORE: &str = r#"#!/bin/sh
case "$1/$2" in
  app/secret) printf 'abc-123\n' ;;
  app/spaced) printf 'has space\n' ;;
  app/backslash) printf '%s\n' 'ends-with\' ;;
  app/unquotable) printf '%s\n' "a'b\$c" ;;
  *) echo "no such item" >&2; exit 3 ;;
esac
"#;

const FAKE_GH: &str = r#"#!/bin/sh
{ printf 'ARGS: %s\n' "$*"; cat; printf '\nEND\n'; } >> "$FAKE_GH_LOG"
"#;

const FAKE_PROTON: &str = r#"#!/bin/sh
[ "$1 $2 $3" = "item view pass://Vault/item/field" ] || { echo "bad args: $*" >&2; exit 2; }
printf 'pr0ton\n'
"#;

const LOCAL_TEMPLATE: &str = "# local\nAPP_ENV=local\nURL=https://api.example.com\n\
                              SECRET=t://app/secret\nSPACED=t://app/spaced\n";

struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    bin: PathBuf,
    config_home: PathBuf,
    gh_log: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let config_home = dir.path().join("config");
        let gh_log = dir.path().join("gh.log");
        let fixture = Fixture {
            _dir: dir,
            repo,
            bin,
            config_home,
            gh_log,
        };
        fixture.user_config(&format!(
            "[stores.t]\nbackend = \"exec\"\ncommand = \"{}\"\nargs = [\"{{0}}\", \"{{1}}\"]\n",
            fixture.bin.join("fake-store").display()
        ));
        fixture.script("fake-store", FAKE_STORE);
        fixture.script("fake-proton", FAKE_PROTON);
        fixture.script("gh", FAKE_GH);
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ]);
        fixture.write(".gitignore", ".env\n");
        fixture.write("config/local.env.tpl", LOCAL_TEMPLATE);
        fixture.write("config/prod.env.tpl", "APP_ENV=prod\nSECRET=t://app/secret\n");
        fixture.write("secretmap.toml", &fixture.default_map());
        fixture.commit();
        fixture
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn user_config(&self, text: &str) {
        let dir = self.config_home.join("secretmap");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), text).unwrap();
    }

    fn stores(&self) -> String {
        "version = 1\nstores = [\"t\"]\n".to_string()
    }

    fn default_map(&self) -> String {
        self.stores()
            + r#"
[[target]]
name = "local"
template = "config/local.env.tpl"
file = ".env"

[[target]]
name = "prod-env"
template = "config/prod.env.tpl"
github = { environment = "production", secret = "ENV_FILE_CONTENT" }

[[target]]
name = "token"
value = "t://app/secret"
github = { secret = "BUILD_TOKEN" }
"#
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.repo.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "fixture"]);
    }

    fn run(&self, args: &[&str]) -> Output {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        Command::new(env!("CARGO_BIN_EXE_secretmap"))
            .current_dir(&self.repo)
            .env("PATH", path)
            .env("FAKE_GH_LOG", &self.gh_log)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        String::from_utf8(out.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?} should fail");
        stderr(&out)
    }

    fn env_file(&self) -> PathBuf {
        self.repo.join(".env")
    }

    fn gh_log(&self) -> String {
        fs::read_to_string(&self.gh_log).unwrap_or_default()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn check_reads_no_secrets() {
    let f = Fixture::new();
    // A store that fails if called proves check never touches it.
    f.script("fake-store", "#!/bin/sh\nexit 9\n");
    let out = f.ok(&["check"]);
    assert!(out.contains("acme/widgets"), "{out}");
    assert!(out.contains("store t:// (exec)"), "{out}");
}

#[test]
fn pull_writes_a_private_file() {
    let f = Fixture::new();
    let out = f.ok(&["pull"]);
    assert!(out.contains("wrote .env (4 keys)"), "{out}");
    let text = fs::read_to_string(f.env_file()).unwrap();
    assert!(text.starts_with("# Generated by secretmap from config/local.env.tpl"));
    assert!(text.contains("\n# local\nAPP_ENV=local\nURL=https://api.example.com\n"));
    assert!(text.contains("\nSECRET=abc-123\nSPACED='has space'\n"));
    assert_eq!(mode(&f.env_file()), 0o600);
    assert_eq!(f.gh_log(), "", "pull must not touch GitHub");
    // Its own output can be regenerated without --force.
    f.ok(&["pull"]);
}

#[test]
fn pull_protects_files_it_did_not_generate() {
    let f = Fixture::new();
    f.write(".env", "MINE=1\n");
    let err = f.fails(&["pull"]);
    assert!(err.contains("didn't generate"), "{err}");
    assert_eq!(fs::read_to_string(f.env_file()).unwrap(), "MINE=1\n");
    f.ok(&["pull", "--force"]);
    assert!(!fs::read_to_string(f.env_file()).unwrap().contains("MINE"));
}

#[test]
fn dry_run_shows_key_names_only_and_writes_nothing() {
    let f = Fixture::new();
    f.write(".env", "SECRET=old-value\nGONE=old-gone\n");
    let out = f.ok(&["pull", "--dry-run", "--force"]);
    assert!(out.contains("added:   APP_ENV, SPACED, URL"), "{out}");
    assert!(out.contains("removed: 1 existing entry"), "{out}");
    assert!(out.contains("changed: SECRET"), "{out}");
    for value in ["abc-123", "old-value", "GONE", "old-gone", "has space"] {
        assert!(!out.contains(value), "{value} leaked: {out}");
    }
    assert_eq!(
        fs::read_to_string(f.env_file()).unwrap(),
        "SECRET=old-value\nGONE=old-gone\n"
    );
}

#[test]
fn dry_run_never_prints_text_from_the_existing_file() {
    let f = Fixture::new();
    f.write(
        ".env",
        "KEY='-----BEGIN\nS3CR3TBASE64TAIL==\nhunter2 = pw\n\
         postgres://u:pa55@db/x?sslmode=require\n",
    );
    let out = f.ok(&["pull", "--dry-run", "--force"]);
    assert!(out.contains("removed: 4 existing entries"), "{out}");
    for leak in ["S3CR3T", "hunter2", "pa55", "KEY"] {
        assert!(!out.contains(leak), "{leak} leaked: {out}");
    }
}

#[test]
fn dry_run_notices_changes_that_keep_the_same_values() {
    let f = Fixture::new();
    one_template(&f, "export A=1\n# note\n", "file=\".env\"");
    f.write(".env", "A=1\n");
    let out = f.ok(&["pull", "--dry-run", "--force"]);
    assert!(
        out.contains("changed: A") || out.contains("differ"),
        "{out}"
    );
    assert!(!out.contains("no changes"), "{out}");

    f.fails(&["pull"]);
    f.ok(&["pull", "--force"]);
    let out = f.ok(&["pull", "--dry-run"]);
    assert!(out.contains(".env: no changes"), "{out}");
}

#[test]
fn pull_uses_uncommitted_template_edits_but_push_refuses_them() {
    let f = Fixture::new();
    one_template(&f, "A=1\n", "github={secret=\"S\"}");
    f.write("config/t.env.tpl", "A=2\n");
    for args in [&["push"][..], &["push", "--dry-run"], &["push", "--force"]] {
        let err = f.fails(args);
        assert!(err.contains("uncommitted changes"), "{err}");
    }
    assert_eq!(f.gh_log(), "");

    // Staged isn't committed either.
    f.git(&["add", "-A"]);
    assert!(f.fails(&["push"]).contains("uncommitted changes"));

    // Committing is enough: the commit needn't be on the remote.
    f.commit();
    f.ok(&["push"]);
    assert!(f.gh_log().contains("A=2\n"), "{}", f.gh_log());

    one_template(&f, "A=1\n", "file=\".env\"");
    f.write("config/t.env.tpl", "A=edited\n");
    f.ok(&["pull"]);
    assert!(fs::read_to_string(f.env_file())
        .unwrap()
        .ends_with("\nA=edited\n"));
}

#[test]
fn a_new_staged_template_cannot_be_pushed() {
    let f = Fixture::new();
    f.write("config/new.env.tpl", "A=1\n");
    f.git(&["add", "config/new.env.tpl"]);
    f.write(
        "secretmap.toml",
        &(f.stores()
            + "[[target]]\nname=\"x\"\ntemplate=\"config/new.env.tpl\"\ngithub={secret=\"S\"}\n"),
    );
    assert!(f.fails(&["push"]).contains("uncommitted changes"));
    assert_eq!(f.gh_log(), "");
}

#[test]
fn glob_characters_in_a_template_path_are_literal() {
    let f = Fixture::new();
    f.write("config/ab.tpl", "A=1\n");
    f.commit();
    f.write("config/a[b].tpl", "PRIVATE=1\n");
    let err = map_error(
        &f,
        "[[target]]\nname=\"x\"\ntemplate=\"config/a[b].tpl\"\ngithub={secret=\"S\"}\n",
    );
    assert!(err.contains("isn't committed"), "{err}");
}

#[test]
fn a_store_failure_writes_nothing() {
    let f = Fixture::new();
    f.write("config/local.env.tpl", "A=t://app/missing\n");
    f.commit();
    let err = f.fails(&["pull"]);
    assert!(err.contains("no such item"), "{err}");
    assert!(!f.env_file().exists());
}

#[test]
fn push_sends_values_over_stdin_to_origin_and_skips_unchanged() {
    let f = Fixture::new();
    let out = f.ok(&["push"]);
    assert!(out.contains("github targets go to acme/widgets (from origin)"), "{out}");
    assert!(out.contains("production/ENV_FILE_CONTENT: pushed"), "{out}");
    assert!(out.contains("repo/BUILD_TOKEN: pushed"), "{out}");
    assert!(!out.contains("abc-123"), "{out}");

    let log = f.gh_log();
    assert!(
        log.contains(
            "ARGS: secret set ENV_FILE_CONTENT --repo github.com/acme/widgets --env production\n\
             APP_ENV=prod\nSECRET=abc-123\n\nEND\n"
        ),
        "{log}"
    );
    assert!(
        log.contains("ARGS: secret set BUILD_TOKEN --repo github.com/acme/widgets\nabc-123\nEND\n"),
        "{log}"
    );
    assert!(!log.contains("Generated by secretmap"), "{log}");
    assert!(!f.env_file().exists(), "push must not write local files");

    let again = f.ok(&["push"]);
    assert!(again.contains("unchanged"), "{again}");
    assert_eq!(f.gh_log(), log, "unchanged values must not be re-sent");

    f.ok(&["push", "--force", "--target", "token"]);
    assert_eq!(f.gh_log().matches("BUILD_TOKEN").count(), 2);
    assert_eq!(f.gh_log().matches("ENV_FILE_CONTENT").count(), 1);
}

#[test]
fn push_dry_run_sends_nothing() {
    let f = Fixture::new();
    let out = f.ok(&["push", "--dry-run"]);
    assert!(out.contains("would push"), "{out}");
    assert_eq!(f.gh_log(), "");
    // A dry run mustn't count as a push.
    assert!(f.ok(&["push"]).contains("pushed"));
}

#[test]
fn push_refuses_a_non_github_origin() {
    let f = Fixture::new();
    f.git(&[
        "remote",
        "set-url",
        "origin",
        "https://gitlab.example.com/a/b.git",
    ]);
    let err = f.fails(&["push"]);
    assert!(err.contains("isn't a github.com repository"), "{err}");
    assert_eq!(f.gh_log(), "");
}

#[test]
fn origin_errors_do_not_print_credentials() {
    let f = Fixture::new();
    f.git(&[
        "remote",
        "set-url",
        "origin",
        "https://user:s3cret@gitlab.example.com/a/b.git",
    ]);
    for command in ["check", "push"] {
        let err = f.fails(&[command]);
        assert!(err.contains("isn't a github.com repository"), "{err}");
        assert!(!err.contains("s3cret"), "{err}");
    }
}

#[test]
fn push_refuses_an_origin_posing_as_github() {
    let f = Fixture::new();
    f.git(&[
        "remote",
        "set-url",
        "origin",
        "https://evil.example/@github.com/acme/widgets",
    ]);
    f.fails(&["push"]);
    assert_eq!(f.gh_log(), "");
}

#[test]
fn pull_writes_into_dot_directories() {
    let f = Fixture::new();
    f.write(".gitignore", ".env\n.config/app.env\n");
    f.write(
        "secretmap.toml",
        &(f.stores()
            + "[[target]]\nname=\"x\"\ntemplate=\"config/local.env.tpl\"\n\
               file=\".config/app.env\"\n"),
    );
    f.ok(&["pull"]);
    let dir = f.repo.join(".config");
    assert_eq!(mode(&dir.join("app.env")), 0o600);
    assert_eq!(fs::read_dir(dir).unwrap().count(), 1, "staging left behind");
}

/// A committed template and a map with one target using it.
fn one_template(f: &Fixture, text: &str, dest: &str) {
    f.write("config/t.env.tpl", text);
    f.write(
        "secretmap.toml",
        &(f.stores() + &format!("[[target]]\nname=\"x\"\ntemplate=\"config/t.env.tpl\"\n{dest}\n")),
    );
    f.commit();
}

#[test]
fn exported_secrets_stay_exported() {
    let f = Fixture::new();
    one_template(
        &f,
        "export SECRET=t://app/secret\nexport LITERAL=1\n",
        "file=\".env\"",
    );
    f.ok(&["pull"]);
    let text = fs::read_to_string(f.env_file()).unwrap();
    assert!(
        text.ends_with("\nexport SECRET=abc-123\nexport LITERAL=1\n"),
        "{text}"
    );
}

#[test]
fn quoted_references_are_refused() {
    let f = Fixture::new();
    one_template(&f, "A=\"t://app/secret\"\n", "file=\".env\"");
    let err = f.fails(&["check"]);
    assert!(err.contains("remove the quotes"), "{err}");
}

#[test]
fn references_to_undeclared_stores_are_refused() {
    let f = Fixture::new();
    // `pass` is built in; this map declares only `t`.
    one_template(&f, "A=pass://Vault/item/field\n", "file=\".env\"");
    let err = f.fails(&["check"]);
    assert!(err.contains("doesn't declare"), "{err}");
    assert!(!f.env_file().exists());

    f.user_config(&format!(
        "[stores.t]\nbackend = \"exec\"\ncommand = \"{0}\"\nargs = [\"{{0}}\", \"{{1}}\"]\n\
         [stores.lp]\nbackend = \"exec\"\ncommand = \"{0}\"\nargs = []\n",
        f.bin.join("fake-store").display()
    ));
    one_template(&f, "A=lp://github/password\n", "file=\".env\"");
    let err = f.fails(&["check"]);
    assert!(err.contains("store lp"), "{err}");
}

#[test]
fn unfamiliar_schemes_are_copied_with_a_warning() {
    let f = Fixture::new();
    one_template(
        &f,
        "BUS=nats://broker:4222\nURL=https://example.com\n",
        "file=\".env\"",
    );
    let out = f.run(&["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("BUS is copied as written"), "{err}");
    assert!(!err.contains("URL"), "{err}");
    f.ok(&["pull"]);
    let text = fs::read_to_string(f.env_file()).unwrap();
    assert!(text.contains("\nBUS=nats://broker:4222\n"), "{text}");
}

#[test]
fn values_parsers_would_misread_are_refused() {
    let f = Fixture::new();
    one_template(&f, "A=t://app/backslash\n", "file=\".env\"");
    let err = f.fails(&["pull"]);
    assert!(err.contains("misread"), "{err}");
    assert!(!err.contains("ends-with"), "{err}");
    assert!(!f.env_file().exists());
}

#[test]
fn one_destination_cannot_be_named_twice() {
    let f = Fixture::new();
    let tpl = "template = \"config/local.env.tpl\"";
    for (first, second) in [
        ("file=\".env\"", "file=\"./.env\""),
        ("github={secret=\"TOKEN\"}", "github={secret=\"token\"}"),
        (
            "github={secret=\"X\",environment=\"Prod\"}",
            "github={secret=\"x\",environment=\"prod\"}",
        ),
    ] {
        let err = map_error(
            &f,
            &format!(
                "[[target]]\nname=\"a\"\n{tpl}\n{first}\n\
                 [[target]]\nname=\"b\"\n{tpl}\n{second}\n"
            ),
        );
        assert!(err.contains("same destination"), "{first}: {err}");
    }
}

#[test]
fn an_environment_called_repo_is_its_own_destination() {
    let f = Fixture::new();
    f.write(
        "secretmap.toml",
        &(f.stores()
            + "[[target]]\nname=\"a\"\nvalue=\"t://app/secret\"\ngithub={secret=\"X\"}\n\
               [[target]]\nname=\"b\"\nvalue=\"t://app/secret\"\n\
               github={secret=\"X\",environment=\"repo\"}\n"),
    );
    f.ok(&["push"]);
    let log = f.gh_log();
    assert!(log.contains("--repo github.com/acme/widgets\n"), "{log}");
    assert!(log.contains("--env repo\n"), "{log}");
    // Each has its own record, so a repeat push skips both.
    f.ok(&["push"]);
    assert_eq!(f.gh_log(), log);
}

#[test]
fn a_value_that_cannot_render_stops_push_before_anything_is_sent() {
    let f = Fixture::new();
    f.write("config/bad.env.tpl", "A=t://app/unquotable\n");
    f.write(
        "secretmap.toml",
        &(f.stores()
            + "[[target]]\nname=\"good\"\nvalue=\"t://app/secret\"\ngithub={secret=\"GOOD\"}\n\
               [[target]]\nname=\"bad\"\ntemplate=\"config/bad.env.tpl\"\n\
               github={secret=\"BAD\"}\n"),
    );
    f.commit();
    f.fails(&["push"]);
    assert_eq!(f.gh_log(), "");
}

#[test]
fn targets_are_selected_by_kind() {
    let f = Fixture::new();
    assert!(f
        .fails(&["pull", "--target", "prod-env"])
        .contains("isn't a file"));
    assert!(f
        .fails(&["push", "--target", "local"])
        .contains("isn't a remote"));
    assert!(f.fails(&["pull", "--target", "nope"]).contains("no target"));
}

fn map_error(f: &Fixture, target: &str) -> String {
    f.write("secretmap.toml", &(f.stores() + target));
    f.fails(&["check"])
}

#[test]
fn ignored_directories_and_git_are_not_destinations() {
    let f = Fixture::new();
    // A commit controls .gitignore, so ignoring these must not make them safe.
    f.write(".gitignore", ".env\ndist/\n.git/\n");
    let tpl = "template = \"config/local.env.tpl\"";

    let err = map_error(
        &f,
        &format!("[[target]]\nname=\"x\"\n{tpl}\nfile=\"dist/app.env\"\n"),
    );
    assert!(err.contains("inside an ignored directory"), "{err}");

    let err = map_error(
        &f,
        &format!("[[target]]\nname=\"x\"\n{tpl}\nfile=\".git/config\"\n"),
    );
    assert!(err.contains("inside .git"), "{err}");
}

#[test]
fn unsafe_maps_are_refused() {
    let f = Fixture::new();
    let tpl = "template = \"config/local.env.tpl\"";

    let err = map_error(
        &f,
        &format!("[[target]]\nname=\"x\"\n{tpl}\nfile=\"../.env\"\n"),
    );
    assert!(err.contains("inside the repository"), "{err}");

    let err = map_error(
        &f,
        &format!("[[target]]\nname=\"x\"\n{tpl}\nfile=\"/tmp/.env\"\n"),
    );
    assert!(err.contains("inside the repository"), "{err}");

    let err = map_error(
        &f,
        &format!("[[target]]\nname=\"x\"\n{tpl}\nfile=\"out.env\"\n"),
    );
    assert!(err.contains("isn't gitignored"), "{err}");

    // Git would read these as `.env`, but they'd be written somewhere else.
    for file in [":(top).env", ":/.env"] {
        let err = map_error(
            &f,
            &format!("[[target]]\nname=\"x\"\n{tpl}\nfile=\"{file}\"\n"),
        );
        assert!(err.contains("':'"), "{file}: {err}");
    }

    // A tracked file is never a destination, even if it also matches .gitignore.
    let err = map_error(
        &f,
        &format!("[[target]]\nname=\"x\"\n{tpl}\nfile=\"config/prod.env.tpl\"\n"),
    );
    assert!(err.contains("isn't gitignored"), "{err}");

    // Local files can't be smuggled out as templates.
    f.write(".env", "PRIVATE=1\n");
    let err = map_error(
        &f,
        "[[target]]\nname=\"x\"\ntemplate=\".env\"\ngithub={secret=\"LEAK\"}\n",
    );
    assert!(err.contains("isn't committed"), "{err}");

    let err = map_error(
        &f,
        "[[target]]\nname=\"x\"\nvalue=\"https://example.com\"\ngithub={secret=\"X\"}\n",
    );
    assert!(err.contains("must reference a declared store"), "{err}");
}

#[test]
fn a_symlinked_destination_is_refused() {
    let f = Fixture::new();
    let outside = f.repo.parent().unwrap().join("outside");
    fs::write(&outside, "keep\n").unwrap();
    std::os::unix::fs::symlink(&outside, f.env_file()).unwrap();
    let err = f.fails(&["pull", "--force"]);
    assert!(err.contains("symlink"), "{err}");
    assert_eq!(fs::read_to_string(outside).unwrap(), "keep\n");
}

#[test]
fn proton_backend_uses_pass_uris_whatever_the_store_is_called() {
    let f = Fixture::new();
    f.user_config(&format!(
        "[stores.pp]\nbackend = \"proton-pass\"\ncommand = \"{}\"\nauth = \"existing-session\"\n",
        f.bin.join("fake-proton").display()
    ));
    f.write(
        "secretmap.toml",
        "version = 1\nstores = [\"pp\"]\n\n[[target]]\nname = \"local\"\n\
         template = \"config/p.env.tpl\"\nfile = \".env\"\n",
    );
    f.write("config/p.env.tpl", "S=pp://Vault/item/field\n");
    f.commit();
    f.ok(&["pull"]);
    assert!(fs::read_to_string(f.env_file())
        .unwrap()
        .ends_with("\nS=pr0ton\n"));
}

#[test]
fn references_cannot_climb_out_of_a_store_directory() {
    let f = Fixture::new();
    // A store that leaves a marker proves it's never reached.
    let marker = f.repo.join("store-ran");
    f.script(
        "fake-store",
        &format!("#!/bin/sh\ntouch {}\nprintf 'x\\n'\n", marker.display()),
    );

    let target = "[[target]]\nname=\"x\"\nvalue=\"t://../../etc/passwd\"\ngithub={secret=\"S\"}\n";
    f.write("secretmap.toml", &(f.stores() + target));
    let err = f.fails(&["push"]);
    assert!(err.contains("malformed reference"), "{err}");

    one_template(&f, "A=t://app/../../secret\n", "file=\".env\"");
    let err = f.fails(&["pull"]);
    assert!(err.contains("malformed reference"), "{err}");

    assert!(!marker.exists());
    assert_eq!(f.gh_log(), "");
    assert!(!f.env_file().exists());
}

#[test]
fn misspelled_store_settings_are_refused() {
    let f = Fixture::new();
    for (config, typo) in [
        (
            "backend = \"proton-pass\"\ncomand = \"/opt/pass-cli\"",
            "comand",
        ),
        (
            "backend = \"proton-pass\"\naut = \"existing-session\"",
            "aut",
        ),
        (
            "backend = \"exec\"\ncommand = \"/bin/echo\"\nargs = []\narg = []",
            "arg",
        ),
    ] {
        f.user_config(&format!("[stores.t]\n{config}\n"));
        let err = f.fails(&["check"]);
        assert!(err.contains(&format!("unknown field `{typo}`")), "{err}");
    }
}

#[test]
fn values_with_a_tilde_survive_being_sourced() {
    let f = Fixture::new();
    one_template(&f, "export T=t://app/secret\n", "file=\".env\"");
    for value in ["~/tok", "a:~/b", "~", "~root/x"] {
        f.script(
            "fake-store",
            &format!("#!/bin/sh\nprintf '%s\\n' '{value}'\n"),
        );
        f.ok(&["pull", "--force"]);
        let out = Command::new("sh")
            .arg("-c")
            .arg(". ./.env; printf %s \"$T\"")
            .current_dir(&f.repo)
            .env("HOME", "/home/someone")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(String::from_utf8(out.stdout).unwrap(), value);
    }
}

#[test]
fn a_map_cannot_choose_what_runs() {
    let f = Fixture::new();
    let marker = f.repo.join("ran");
    f.script("evil", &format!("#!/bin/sh\ntouch {}\n", marker.display()));
    f.write(
        "secretmap.toml",
        &format!(
            "version = 1\n[stores.t]\nbackend = \"exec\"\ncommand = \"{}\"\nargs = []\n",
            f.bin.join("evil").display()
        ),
    );
    f.fails(&["pull"]);
    assert!(!marker.exists());
}

#[test]
fn an_undefined_store_is_reported() {
    let f = Fixture::new();
    f.user_config("");
    let err = f.fails(&["check"]);
    assert!(err.contains("needs store t"), "{err}");
}
