# Configuration

## Reloading configuration

All config lives in plain TOML files on disk. After editing them, pick up
the changes with one of:

- TUI: `/config reload`
- CLI: `tau config reload`

The command re-reads:

- `~/.config/tau/providers.toml` — the provider list and their custom models.
- `~/.config/tau/models.toml` — global model aliases.

And rebuilds the daemon's in-memory provider / model tables. If the file
has a parse error the reload is rejected and the currently-loaded config
stays in effect, so a broken edit can't brick a running server.

Reloads do **not** change the model snapshot already stored on
existing sessions — sessions hold on to the `Model` they were created
with. New sessions (and model lookups after reload) pick up the updated
config.

The following sources are re-read on every use and need **no** reload:

- Per-project `.tau/models.toml` (read on every session create / alias lookup).
- Per-project operator aliases at `~/.config/tau/projects/{name}/models.toml`.
- `auth.json` (read on every outgoing API request).

`/config show` and `tau config show` print the file paths plus whether
each currently exists — handy for answering "which file am I editing?".

## Provider configuration

Tau loads its provider list from `~/.config/tau/providers.toml` (the path
honours `$XDG_CONFIG_HOME`). Built-in providers (anthropic, openai) are
merged with anything you add here.

```toml
[providers.openai]
api = "openai"
base_url = "https://api.openai.com/v1"
api_key = "$OPENAI_API_KEY"

[[providers.openai.models]]
id = "gpt-4.1"
name = "GPT-4.1"
context_window = 1047576
max_tokens = 32768
```

`api_key` may be inline, `"$ENV_VAR"` for environment expansion, or
`"none"` / omitted to disable the provider's inline credential.

## Model aliases

Aliases let you refer to a model by **role** (`smart`, `fast`, `cheap`,
`worker`, …) instead of by version string. They live in two places,
both named `models.toml`:

| Scope   | File                                       |
|---------|--------------------------------------------|
| Global  | `~/.config/tau/models.toml`                |
| Project | `{project}/.tau/models.toml`               |

> **Migration.** Earlier versions of tau kept the global `[aliases]`
> section in `~/.config/tau/providers.toml`. That location is now
> deprecated. Tau still reads it as a fallback but prints a warning on
> startup; move your aliases to `~/.config/tau/models.toml` to silence
> it.

### Global aliases

Add an `[aliases]` section to `~/.config/tau/models.toml` (the path
honours `$XDG_CONFIG_HOME`):

```toml
[aliases]
smart = "claude-opus-4-6"
fast  = "claude-haiku-4"
cheap = "openai/gpt-4.1-mini"
```

The value is a model id, optionally prefixed with `provider/` to
disambiguate when the same id exists under multiple providers.

### Per-project aliases

Per-project overrides live in `{project}/.tau/models.toml`. The format is
identical:

```toml
# .tau/models.toml — committed alongside the project
[aliases]
smart    = "claude-sonnet-4"
worker   = "gpt-5.1-codex"
reviewer = "claude-opus-4-6"
```

Project aliases override global aliases with the same name. Sessions
created with a `cwd` inside the project (or with no explicit `cwd` so
that the parent's `cwd` is inherited) will pick up that project's
aliases.

### Resolution order

When you say `tau chat -m smart` or `/model smart`, the resolver:

1. Looks `smart` up in the **project** alias map (loaded from
   `{cwd}/.tau/models.toml`, if `cwd` is set).
2. Falls back to the **global** alias map
   (`~/.config/tau/models.toml [aliases]`).
3. Falls back to treating `smart` as a literal model id.

Only **one alias hop** is performed: alias targets must be model ids,
not other aliases. This makes cycles impossible by construction.

### Collisions

If an alias name happens to match a real model id, the **alias wins**.
This lets you redirect a model id to a proxied or wrapped variant
without renaming the upstream id.

### `provider/model-id` parsing

Targets like `"openai/gpt-4.1-mini"` are split on the **first** `/`
only — `provider="openai"`, `id="gpt-4.1-mini"`. Unusual model ids that
themselves contain `/` are still preserved in the id half.

When a request supplies an explicit `--provider` (or `provider_name`
field) it takes precedence over the alias-target's `provider/` prefix.

### Errors

- A configured alias whose target does not exist is a **hard error**
  rather than a silent fallback. The user gets a message like
  `global alias 'smart' points at unknown model 'ghost'. Use `tau models`
  to list available models.`
- A literal model id that does not exist falls through to the parent
  session's model and finally to the server-wide default — same as
  before aliases existed.

## Listing aliases

```sh
tau models
```

Lists all known models followed by:
- `aliases (global):` — entries from `~/.config/tau/models.toml`
- `aliases (project):` — entries from `./.tau/models.toml` (only when
  run from the project root; the lookup is non-recursive in v1)

Inside a session you can also run `/model` (no arguments) to see the
same information.

## Project instructions

Unrelated but adjacent: `{project}/.tau/instructions.toml` lets you
inject prompt fragments into task lifecycle phases. See
`crates/tau-agent-plugin-tasks/src/tasks_config.rs` for the format.

## Logging

The tau server daemon writes all diagnostic output to a rolling log file
under the XDG state directory:

```
~/.local/state/tau/logs/server.log           # current day
~/.local/state/tau/logs/server.log.YYYY-MM-DD # rotated daily
~/.local/state/tau/logs/daemon.stderr.log    # catch-all for panics / raw stderr
```

(The state directory honours `$XDG_STATE_HOME`; falls back to
`~/.local/state/tau`, then `/tmp/tau-state`.)

Files older than 14 days are deleted automatically at server startup.

### Adjusting verbosity

The log level is controlled by the `TAU_LOG` environment variable.
Standard [`tracing-subscriber` filter syntax][filter-syntax] is supported
— set module paths to the desired level, comma-separated:

```sh
TAU_LOG=info                                       # default
TAU_LOG=warn                                       # quieter
TAU_LOG=debug                                      # very chatty
TAU_LOG=tau_agent_lib::plugin=debug,tau_agent_lib=info     # per-module
```

`TAU_LOG` takes precedence. If it's unset, tau falls back to `RUST_LOG`,
and otherwise defaults to `info`.

[filter-syntax]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html

### Debugging a hung plugin

Plugins (global or per-session) have their stderr forwarded into the
server log in real time. If `tau chat` appears to hang, tail the log:

```sh
tail -f ~/.local/state/tau/logs/server.log
```

Look for the last `plugin.hook` span with no matching completion — that
plugin is the culprit. `TAU_LOG=tau_agent_lib::plugin=debug` enables the
`sending` / `returned` traces inside each hook span, making it obvious
where in the round-trip the plugin got stuck.

## Diagnostics scan (`diagnostics.toml`)

The `diagnostics_scan` worker tool runs a configured linter / type-checker
against specific files and returns structured per-file feedback. By
default it ships with built-in support for Rust via `cargo check
--message-format=json`; other languages are wired up through a config
file.

Resolution order (highest priority first):

| Tier    | Path                                               |
|---------|----------------------------------------------------|
| Project | `{project}/.tau/diagnostics.toml`                  |
| Global  | `~/.config/tau/diagnostics.toml`                   |

The first tier where a file exists wins (no merging across tiers — pick
one). When neither tier has a config, only the built-in Rust provider
is active.

### Schema

```toml
# Each [[providers]] entry maps a set of file extensions to a
# diagnostics provider. The first matching provider wins; if a
# `*.rs` provider is not declared, the built-in `rust-cargo-check`
# is used automatically.

[[providers]]
extensions = ["rs"]
builtin = "rust-cargo-check"  # the only built-in for v1

[[providers]]
extensions = ["py"]
cmd = ["ruff", "check", "--output-format=json", "{file}"]
format = "ruff-json"
timeout_secs = 30
name = "ruff"

[[providers]]
extensions = ["ts", "tsx"]
cmd = ["npx", "tsc", "--noEmit", "--pretty", "false", "{file}"]
format = "tsc-text"
```

Field reference:

- `extensions` — list of file extensions (without leading dot, case
  insensitive). Files whose extension is not covered by any provider
  are reported as `skipped` with reason `no diagnostics provider for
  .ext`.
- `builtin` — name of a built-in provider. Currently the only valid
  value is `"rust-cargo-check"`.
- `cmd` — argv template for an external command. The literal token
  `{file}` is substituted with the absolute file path; if no `{file}`
  token is present, the path is appended as the last argument.
- `cwd` — optional working directory (resolved relative to the
  project root when not absolute). Defaults to the session cwd.
- `format` — required when `cmd` is set. One of:
  - `"cargo-json"` — `cargo`'s `compiler-message` JSON stream (one
    JSON object per line).
  - `"rustc-json"` — `rustc --error-format=json`. *Parser stub in
    v1; emits a `TAU0002` "not yet implemented" diagnostic.*
  - `"ruff-json"` — `ruff --output-format=json`. *Parser stub in v1.*
  - `"tsc-text"` — `tsc --pretty false` text output. *Parser stub in
    v1.*
  - `"eslint-json"` — `eslint --format json`. *Parser stub in v1.*
- `timeout_secs` — per-file hard timeout for the provider (default
  60). On timeout the tool emits a `TAU0001` error diagnostic on
  the file rather than failing the whole call.
- `name` — optional human-readable identifier used in error messages
  and to deduplicate command providers when scanning multiple files.

### Output

The tool returns a JSON document of the form:

```json
{
  "summary": {"errors": 1, "warnings": 0, "info": 0,
              "files_scanned": 1, "files_skipped": 1},
  "diagnostics": [
    {"path": "src/lib.rs", "line": 42, "column": 5,
     "severity": "error", "code": "E0308",
     "message": "mismatched types", "source": "rustc"}
  ],
  "skipped": [
    {"path": "README.md", "reason": "no diagnostics provider for .md"}
  ]
}
```

`is_error` on the tool result is `false` even when diagnostics are
present — the tool *succeeded*, the project has problems. `is_error`
is reserved for genuine tool failures (the provider command not being
on `PATH`, etc.), which surface as `skipped` entries.

### Caveats

- **`cargo check` cost**: the built-in Rust provider runs `cargo
  check` over the entire workspace each call. This is typically 2–10 s
  warm-cache for a tau-sized workspace. To trade accuracy for speed,
  override the `*.rs` provider with a custom `cmd` (e.g. `cargo check
  -p mycrate`).
- **Cargo lock contention**: if you also have `cargo build` or another
  `cargo check` running (e.g. from a `bash` tool call), the
  `diagnostics_scan` invocation will serialize on the workspace
  build-script lock. Avoid running both concurrently.
- **Workspace detection**: the built-in walks up from each file to the
  nearest `Cargo.toml` and prefers an ancestor with `[workspace]`.
  Files with no `Cargo.toml` ancestor are reported as `skipped`.
