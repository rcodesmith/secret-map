# secretmap

Keep secrets in one store (Proton Pass first) and every project's config in git.
`secretmap` renders each project's config from a committed **map** and **templates**,
fetching only the secret values from the store, and delivers it to **destinations**:
local env files, or remote secret managers. GitHub Actions secrets are the first
remote destination; others (such as AWS) can be added the same way.

- **One source per value.** Non-secret config is a literal in a committed template.
  Each secret is one item in your store, and every project that needs it refers to it.
- **No standing access.** By default each run prompts for a Proton Pass access token,
  uses a throwaway session, and logs out. Nothing on the machine can read the store
  between runs.
- **Pluggable stores.** A store is chosen by the reference's scheme. Proton Pass is
  built in; any CLI that prints a secret to stdout works through the `exec` backend,
  configured in your own config file, never in a repository.
- **Pluggable destinations.** Each target names one destination. `pull` writes local
  files; `push` sends to remote ones. GitHub is the only remote destination so far.

## Quick start

```sh
cargo install --path .
cd my-project
secretmap check                    # validate the map; reads no secrets
secretmap pull --dry-run           # what would change in local files, by key name
secretmap pull                     # write local env files
secretmap push --target production # send to a remote destination
```

## The map

`secretmap.toml` at the repository root (or `--map PATH`). Paths are relative to the
repository root.

```toml
version = 1

# The reference schemes this project uses. Default: ["pass"].
stores = ["pass"]

[[target]]
name = "local"
template = "config/local.env.tpl"
file = ".env"

[[target]]
name = "production"
template = "config/production.env.tpl"
github = { environment = "production", secret = "ENV_FILE_CONTENT" }

[[target]]
name = "build-token"
value = "pass://Shared/package-registry/token"
github = { secret = "PACKAGE_TOKEN" }   # a repository secret
```

Each target has one source (`template`, or `value` for a single secret) and one
destination.

## Destinations

| Key      | Command | Where it goes |
|----------|---------|---------------|
| `file`   | `pull`  | A gitignored file in the repository, written at mode 0600. Needs a `template`. |
| `github` | `push`  | A GitHub Actions secret in the repository `origin` points to: `secret` names it, and `environment` (optional) puts it in a GitHub Environment instead of the repository. |

What happens to a value after it reaches a remote destination is up to you. secretmap
only delivers it.

The map says which secret to set, never where the remote side lives: which
repository, account or region comes from the repository or your own config. For
`github` it is `origin`, which must be on github.com. Values go to `gh` over stdin,
never as arguments.

## Stores

`pass` works out of the box: Proton Pass through `pass-cli`, prompting for a token.
Anything else is defined in `~/.config/secretmap/config.toml` (or under
`$XDG_CONFIG_HOME`), where the key is the reference scheme:

```toml
[stores.pass]
backend = "proton-pass"
command = "/opt/homebrew/bin/pass-cli"   # default: pass-cli on PATH
auth = "pat-prompt"                      # or "existing-session"
```

These settings can't go in the map. They decide which program runs and which
credentials it receives, and a repository must not control that.

## Templates

Env-file syntax. A value that is exactly `<store>://<path>`, unquoted, is fetched;
every other line, comments included, is copied as written. A reference to a store
you have defined but the map doesn't declare is an error. Any other unfamiliar
`scheme://` value is copied with a warning.

```sh
# config/local.env.tpl
APP_ENV=local
API_URL=https://api.example.com          # https isn't a store, so it's a literal
CLIENT_SECRET=pass://App-Prod/oauth/client_secret
```

Fetched values are quoted if they need it. `export KEY=...` stays exported. Values
that env-file parsers can't all read back unchanged are refused: multi-line values,
values ending in `\` or containing `\\`, and values mixing `'` with `"`, `\`, `$` or
a backtick.

## Safety rules

The map is treated as untrusted input, since anyone who can commit to the repository
can change it. secretmap refuses to:

- run a command the map names, or accept store settings from it;
- write a file outside the repository, through a symlink, inside `.git`, or anywhere git
  doesn't ignore;
- write a file inside an ignored directory such as `dist/`, which may get published. Ignore
  the file itself instead;
- read a template git doesn't track, so a map can't turn an arbitrary local file into
  a remote secret;
- push a template that differs from your latest local commit. `pull` uses your working
  copy, so you can edit and try a template before committing it;
- push to a repository, account or region the map names. For `github`, values go only
  to `origin` on github.com;
- overwrite a file it didn't generate, unless you pass `--force`;
- print a secret. `--dry-run` and every message show key names only.

Generated files are written atomically at mode 0600.

Remote secrets often can't be read back, so `push` keeps a salted digest of what it
last pushed to each destination in `.git/secretmap/push-state.toml` and skips
unchanged values. Use `--force` if a secret was changed some other way.

## Proton Pass setup

1. Put project secrets in their own vaults, for example one per project and environment
   plus a shared one. Keep personal items elsewhere.
2. Create a token and grant it read access to only those vaults:

   ```sh
   pass-cli pat create --name secretmap --expiration 3m
   pass-cli pat access grant ...      # viewer on each project vault
   ```

3. Keep the token in your main Proton Pass, and paste it when secretmap asks.

References use pass-cli's URI format: `pass://<vault>/<item>/<field>`.

## Other stores with `exec`

In `~/.config/secretmap/config.toml`, with `stores = ["lp"]` in the project's map:

```toml
# github.com/reyamira/pass-cli, a different tool also called pass-cli
[stores.lp]
backend = "exec"
command = "/usr/local/bin/pass-cli"
args = ["get", "{0}", "--quiet", "--field", "{1}"]   # lp://github/password
```

`{path}` is everything after `scheme://`, and `{0}`, `{1}`, ... are its `/`-separated
segments. No segment may start with `-` or be `.` or `..`.

## Adding a built-in store backend

Implement `store::Store` (`fetch` many paths in one session) in a new module under
`src/store/`, add a variant to `map::StoreConfig`, and construct it in `store::build`.

## Adding a destination

Add a module under `src/dest/` (see `src/dest/github.rs`) with the destination's map
syntax, validation and a way to set a value. Then add its key to `map::RawTarget`, a
variant to `dest::Remote`, and any context it needs, such as an account or region
taken from the repository or your own config rather than the map, to `dest::Remotes`.

## Status

Early. The Proton Pass backend assumes `pass-cli item view pass://vault/item/field`
prints the bare field value, and that `PROTON_PASS_SESSION_DIR` isolates the session.
Both need confirming against a real vault.

## License

Apache-2.0. See [LICENSE](LICENSE).
