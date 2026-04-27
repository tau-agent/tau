//! Diagnostics scan tool — typed lint/syntax feedback for specific files.
//!
//! Runs a configured diagnostics provider (linter / type-checker) against
//! a small set of files and returns a structured JSON payload of
//! per-line `{path, line, column, severity, code?, message, source?}`
//! entries plus a count summary. Built-in support is provided for Rust
//! via `cargo check --message-format=json`; other languages can be wired
//! up via `.tau/diagnostics.toml` (project-tier) or
//! `~/.config/tau/diagnostics.toml` (global tier).
//!
//! See `docs/CONFIG.md` for the config schema.

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::{Pid, setsid};
use serde::{Deserialize, Serialize};

use super::{ToolDef, ToolOutput, resolve_path};
use tau_agent_plugin::{CancelToken, Tool};

// ---------------------------------------------------------------------------
// Public schema (what the model sends and receives)
// ---------------------------------------------------------------------------

/// Maximum number of paths accepted per call. Mirrors `read::MAX_PATHS`.
pub const MAX_PATHS: usize = 20;

/// Default per-provider hard timeout, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// One unified diagnostic entry returned to the model.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct Diagnostic {
    path: String,
    line: u32,
    column: u32,
    severity: Severity,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize)]
struct Skipped {
    path: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
struct Summary {
    errors: usize,
    warnings: usize,
    info: usize,
    files_scanned: usize,
    files_skipped: usize,
}

#[derive(Debug, Clone, Serialize)]
struct ScanResult {
    summary: Summary,
    diagnostics: Vec<Diagnostic>,
    skipped: Vec<Skipped>,
}

// ---------------------------------------------------------------------------
// Tool definition / dispatch
// ---------------------------------------------------------------------------

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "diagnostics_scan".into(),
            description:
                "Run lint/syntax diagnostics on the given files and return structured per-file results. Use this after edits instead of bashing project-wide `cargo check` / `tsc` / `ruff`.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_PATHS,
                        "description": "Files to scan. Paths are resolved relative to the session cwd."
                    }
                },
                "required": ["paths"]
            }),
        },
        execute: Box::new(execute),
        prepare_arguments: None,
    }
}

fn execute(args: serde_json::Value, cwd: &str, cancel: &CancelToken) -> ToolOutput {
    let paths = match args.get("paths").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return ToolOutput::error("missing or empty 'paths' argument"),
    };

    if paths.len() > MAX_PATHS {
        return ToolOutput::error(format!(
            "too many paths: {} (max {})",
            paths.len(),
            MAX_PATHS
        ));
    }

    let mut requested: Vec<(String, PathBuf)> = Vec::new();
    for v in paths {
        let Some(s) = v.as_str() else {
            return ToolOutput::error("'paths' entries must be strings");
        };
        let resolved = resolve_path(cwd, s);
        requested.push((s.to_string(), resolved));
    }

    let config = load_config(cwd);

    let mut result = ScanResult {
        summary: Summary {
            errors: 0,
            warnings: 0,
            info: 0,
            files_scanned: 0,
            files_skipped: 0,
        },
        diagnostics: Vec::new(),
        skipped: Vec::new(),
    };

    // Group files by provider so we can run e.g. `cargo check` once per
    // workspace instead of once per file.
    let mut by_provider: BTreeMap<ProviderKey, Vec<(String, PathBuf)>> = BTreeMap::new();

    for (orig, abs) in requested {
        match abs.try_exists() {
            Ok(true) => {}
            Ok(false) => {
                result.skipped.push(Skipped {
                    path: orig.clone(),
                    reason: "file not found".into(),
                });
                continue;
            }
            Err(e) => {
                result.skipped.push(Skipped {
                    path: orig.clone(),
                    reason: format!("cannot access path: {}", e),
                });
                continue;
            }
        }

        let ext = abs
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let provider = config.lookup(&ext);
        match provider {
            Some(p) => by_provider
                .entry(provider_key(&p))
                .or_default()
                .push((orig, abs)),
            None => {
                let reason = if ext.is_empty() {
                    "no diagnostics provider for files without an extension".into()
                } else {
                    format!("no diagnostics provider for .{}", ext)
                };
                result.skipped.push(Skipped { path: orig, reason });
            }
        }
    }

    for (key, files) in by_provider {
        if cancel.is_cancelled() {
            for (orig, _) in files {
                result.skipped.push(Skipped {
                    path: orig,
                    reason: "scan cancelled".into(),
                });
            }
            continue;
        }
        let provider = config
            .by_key(&key)
            .expect("by_provider key was sourced from config");
        run_provider(&provider, &files, cancel, &mut result);
    }

    finalize(&mut result);
    render(&result)
}

fn finalize(result: &mut ScanResult) {
    result
        .diagnostics
        .sort_by(|a, b| (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column)));
    let mut errors = 0;
    let mut warnings = 0;
    let mut info = 0;
    for d in &result.diagnostics {
        match d.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Info => info += 1,
        }
    }
    result.summary.errors = errors;
    result.summary.warnings = warnings;
    result.summary.info = info;
    result.summary.files_skipped = result.skipped.len();
}

fn render(result: &ScanResult) -> ToolOutput {
    let body = match serde_json::to_string_pretty(result) {
        Ok(s) => s,
        Err(e) => return ToolOutput::error(format!("failed to serialise result: {}", e)),
    };

    let summary = format!(
        "diagnostics_scan: {} error{}, {} warning{} ({} file{} scanned, {} skipped)",
        result.summary.errors,
        if result.summary.errors == 1 { "" } else { "s" },
        result.summary.warnings,
        if result.summary.warnings == 1 {
            ""
        } else {
            "s"
        },
        result.summary.files_scanned,
        if result.summary.files_scanned == 1 {
            ""
        } else {
            "s"
        },
        result.summary.files_skipped,
    );

    // is_error remains false even when diagnostics are present: the tool
    // *succeeded*; the project has problems. is_error is reserved for
    // genuine tool failures (provider didn't run at all etc.) — those
    // are surfaced as `skipped` entries today.
    ToolOutput::text(body).with_summary(summary)
}

// ---------------------------------------------------------------------------
// Provider model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Provider {
    /// Built-in. `cargo check --message-format=json --quiet --keep-going`
    /// from the nearest workspace root.
    RustCargoCheck,
    /// Generic external command. The `cmd` template substitutes `{file}`
    /// with the absolute file path; if no `{file}` token is present, the
    /// path is appended as the last arg.
    Command {
        name: String,
        cmd: Vec<String>,
        cwd: Option<PathBuf>,
        format: ParserKind,
        timeout_secs: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ProviderKey {
    RustCargoCheck,
    Command(String),
}

fn provider_key(p: &Provider) -> ProviderKey {
    match p {
        Provider::RustCargoCheck => ProviderKey::RustCargoCheck,
        Provider::Command { name, .. } => ProviderKey::Command(name.clone()),
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ParserKind {
    /// `cargo` JSON stream — one `{ "reason": ..., "message": ... }`
    /// object per line.
    CargoJson,
    /// `rustc --error-format=json` — each line is the inner `message`
    /// shape (same as `cargo`'s `message` field).
    RustcJson,
    /// `ruff check --output-format=json` — array of objects.
    RuffJson,
    /// `tsc --pretty false` text — `path(line,col): severity TSxxxx: msg`.
    TscText,
    /// `eslint --format json` — array of `{filePath, messages: [...]}`.
    EslintJson,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    providers: Vec<ConfigProvider>,
}

#[derive(Debug, Deserialize)]
struct ConfigProvider {
    extensions: Vec<String>,
    /// One of: `"rust-cargo-check"` (the only built-in for v1).
    #[serde(default)]
    builtin: Option<String>,
    /// Generic command template — used when `builtin` is unset.
    #[serde(default)]
    cmd: Option<Vec<String>>,
    /// Optional working directory (resolved relative to project root).
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    format: Option<ParserKind>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Optional human-readable name used to deduplicate providers when
    /// the same generic command is used for multiple extensions.
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Default)]
struct LoadedConfig {
    /// First-match-wins by extension lookup.
    by_ext: BTreeMap<String, Provider>,
    /// Reverse map: provider key -> Provider, for re-dispatch after grouping.
    providers: Vec<Provider>,
}

impl LoadedConfig {
    fn lookup(&self, ext: &str) -> Option<Provider> {
        self.by_ext.get(ext).cloned()
    }

    fn by_key(&self, key: &ProviderKey) -> Option<Provider> {
        self.providers
            .iter()
            .find(|p| provider_key(p) == *key)
            .cloned()
    }
}

fn load_config(cwd: &str) -> LoadedConfig {
    let mut loaded = LoadedConfig::default();

    // Two-tier resolution: project ({cwd}/.tau/diagnostics.toml) and global
    // (~/.config/tau/diagnostics.toml). Operator tier requires a project
    // name which the tool executor signature does not provide today.
    let from_disk: Option<ConfigFile> =
        tau_agent_base::config_chain::load_first(None, Some(cwd), "diagnostics.toml", true);

    let mut providers: Vec<ConfigProvider> = Vec::new();
    if let Some(cfg) = from_disk {
        providers.extend(cfg.providers);
    }

    // The built-in Rust provider is always available unless the user
    // explicitly maps `*.rs` to something else (the loop below assigns
    // by_ext on first match, so a user-defined `rs` provider wins).
    let have_rs = providers
        .iter()
        .any(|p| p.extensions.iter().any(|e| e.eq_ignore_ascii_case("rs")));
    if !have_rs {
        providers.push(ConfigProvider {
            extensions: vec!["rs".into()],
            builtin: Some("rust-cargo-check".into()),
            cmd: None,
            cwd: None,
            format: None,
            timeout_secs: None,
            name: Some("rust-cargo-check".into()),
        });
    }

    let project_root = PathBuf::from(cwd);

    for cp in providers {
        let provider = match (cp.builtin.as_deref(), cp.cmd.as_ref()) {
            (Some("rust-cargo-check"), _) => Provider::RustCargoCheck,
            (Some(other), _) => {
                eprintln!(
                    "diagnostics_scan: unknown builtin provider '{}', skipping",
                    other
                );
                continue;
            }
            (None, Some(cmd)) if !cmd.is_empty() => {
                let format = match cp.format {
                    Some(f) => f,
                    None => {
                        eprintln!(
                            "diagnostics_scan: provider for {:?} missing 'format', skipping",
                            cp.extensions
                        );
                        continue;
                    }
                };
                let resolved_cwd = cp.cwd.as_ref().map(|s| {
                    let p = Path::new(s);
                    if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        project_root.join(p)
                    }
                });
                let name = cp
                    .name
                    .clone()
                    .unwrap_or_else(|| cmd.first().cloned().unwrap_or_else(|| "command".into()));
                Provider::Command {
                    name,
                    cmd: cmd.clone(),
                    cwd: resolved_cwd,
                    format,
                    timeout_secs: cp.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
                }
            }
            _ => {
                eprintln!(
                    "diagnostics_scan: provider for {:?} has neither 'builtin' nor 'cmd', skipping",
                    cp.extensions
                );
                continue;
            }
        };

        loaded.providers.push(provider.clone());
        for ext in cp.extensions {
            let key = ext.trim_start_matches('.').to_ascii_lowercase();
            loaded.by_ext.entry(key).or_insert_with(|| provider.clone());
        }
    }

    loaded
}

// ---------------------------------------------------------------------------
// Provider execution
// ---------------------------------------------------------------------------

fn run_provider(
    provider: &Provider,
    files: &[(String, PathBuf)],
    cancel: &CancelToken,
    result: &mut ScanResult,
) {
    match provider {
        Provider::RustCargoCheck => run_rust_cargo_check(files, cancel, result),
        Provider::Command {
            name,
            cmd,
            cwd,
            format,
            timeout_secs,
        } => run_command_provider(
            name,
            cmd,
            cwd.as_deref(),
            *format,
            *timeout_secs,
            files,
            cancel,
            result,
        ),
    }
}

fn run_rust_cargo_check(
    files: &[(String, PathBuf)],
    cancel: &CancelToken,
    result: &mut ScanResult,
) {
    // Group files by their workspace root.
    let mut by_root: BTreeMap<PathBuf, Vec<(String, PathBuf)>> = BTreeMap::new();
    for (orig, abs) in files {
        match find_cargo_workspace_root(abs) {
            Some(root) => by_root
                .entry(root)
                .or_default()
                .push((orig.clone(), abs.clone())),
            None => {
                result.skipped.push(Skipped {
                    path: orig.clone(),
                    reason: "no Cargo.toml ancestor — cannot run cargo check".into(),
                });
            }
        }
    }

    for (root, group) in by_root {
        if cancel.is_cancelled() {
            for (orig, _) in group {
                result.skipped.push(Skipped {
                    path: orig,
                    reason: "scan cancelled".into(),
                });
            }
            continue;
        }

        let mut cmd = std::process::Command::new("cargo");
        cmd.arg("check")
            .arg("--message-format=json")
            .arg("--quiet")
            .arg("--keep-going")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CARGO_TERM_COLOR", "never");

        let CmdOutcome {
            stdout,
            stderr,
            status,
            timed_out,
            cancelled,
        } = match spawn_and_wait(cmd, DEFAULT_TIMEOUT_SECS, cancel) {
            Ok(o) => o,
            Err(e) => {
                for (orig, _) in &group {
                    result.skipped.push(Skipped {
                        path: orig.clone(),
                        reason: format!("provider 'rust-cargo-check' not available: {}", e),
                    });
                }
                continue;
            }
        };

        if cancelled {
            for (orig, _) in &group {
                result.skipped.push(Skipped {
                    path: orig.clone(),
                    reason: "scan cancelled".into(),
                });
            }
            continue;
        }

        if timed_out {
            for (orig, _) in &group {
                result
                    .diagnostics
                    .push(timeout_diag(orig, DEFAULT_TIMEOUT_SECS));
            }
            result.summary.files_scanned += group.len();
            continue;
        }

        // Parse stdout for compiler-message reasons.
        let parsed = parse_cargo_json(&stdout);

        // Map abs paths back to the user's original path strings.
        let mut wanted: BTreeMap<PathBuf, String> = BTreeMap::new();
        for (orig, abs) in &group {
            let canon = std::fs::canonicalize(abs).unwrap_or_else(|_| abs.clone());
            wanted.insert(canon, orig.clone());
        }

        let mut emitted_for: std::collections::HashSet<String> = std::collections::HashSet::new();

        for diag in parsed {
            // diag.path is whatever cargo emitted — typically relative to
            // the workspace root. Resolve against the root and canonicalise.
            let abs = if Path::new(&diag.path).is_absolute() {
                PathBuf::from(&diag.path)
            } else {
                root.join(&diag.path)
            };
            let canon = std::fs::canonicalize(&abs).unwrap_or(abs);
            if let Some(orig) = wanted.get(&canon) {
                let mut d = diag;
                d.path = orig.clone();
                emitted_for.insert(orig.clone());
                result.diagnostics.push(d);
            }
        }

        // If `cargo check` failed but emitted no parseable diagnostics
        // for the requested files, surface stderr so the model isn't
        // left wondering. Only do this if the exit code was non-zero
        // *and* we got nothing for any of the requested files — a
        // successful check with zero diagnostics is the happy path.
        let any_emitted = !emitted_for.is_empty();
        if status != 0 && !any_emitted {
            let trimmed = stderr.trim();
            let reason = if trimmed.is_empty() {
                format!(
                    "cargo check failed (exit {}) with no parseable diagnostics",
                    status
                )
            } else {
                let snippet: String = trimmed.lines().take(5).collect::<Vec<_>>().join("\n");
                format!("cargo check failed (exit {}): {}", status, snippet)
            };
            for (orig, _) in &group {
                result.skipped.push(Skipped {
                    path: orig.clone(),
                    reason: reason.clone(),
                });
            }
        } else {
            result.summary.files_scanned += group.len();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_command_provider(
    name: &str,
    cmd_template: &[String],
    cwd_override: Option<&Path>,
    format: ParserKind,
    timeout_secs: u64,
    files: &[(String, PathBuf)],
    cancel: &CancelToken,
    result: &mut ScanResult,
) {
    if cmd_template.is_empty() {
        for (orig, _) in files {
            result.skipped.push(Skipped {
                path: orig.clone(),
                reason: format!("provider '{}' has empty command", name),
            });
        }
        return;
    }

    for (orig, abs) in files {
        if cancel.is_cancelled() {
            result.skipped.push(Skipped {
                path: orig.clone(),
                reason: "scan cancelled".into(),
            });
            continue;
        }

        let abs_str = abs.to_string_lossy().to_string();
        let mut argv: Vec<String> = Vec::with_capacity(cmd_template.len() + 1);
        let mut substituted = false;
        for tok in cmd_template {
            if tok.contains("{file}") {
                argv.push(tok.replace("{file}", &abs_str));
                substituted = true;
            } else {
                argv.push(tok.clone());
            }
        }
        if !substituted {
            argv.push(abs_str.clone());
        }

        let prog = argv.first().cloned().unwrap_or_default();
        let rest: Vec<String> = argv.iter().skip(1).cloned().collect();
        let mut cmd = std::process::Command::new(&prog);
        cmd.args(&rest)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd_override {
            cmd.current_dir(cwd);
        }

        let outcome = match spawn_and_wait(cmd, timeout_secs, cancel) {
            Ok(o) => o,
            Err(e) => {
                result.skipped.push(Skipped {
                    path: orig.clone(),
                    reason: format!("provider '{}' not available: {}", name, e),
                });
                continue;
            }
        };

        if outcome.cancelled {
            result.skipped.push(Skipped {
                path: orig.clone(),
                reason: "scan cancelled".into(),
            });
            continue;
        }

        if outcome.timed_out {
            result.diagnostics.push(timeout_diag(orig, timeout_secs));
            result.summary.files_scanned += 1;
            continue;
        }

        match format {
            ParserKind::CargoJson => {
                let mut parsed = parse_cargo_json(&outcome.stdout);
                for d in &mut parsed {
                    d.path = orig.clone();
                }
                if !parsed.is_empty() {
                    result.diagnostics.extend(parsed);
                }
                result.summary.files_scanned += 1;
            }
            ParserKind::RustcJson
            | ParserKind::RuffJson
            | ParserKind::TscText
            | ParserKind::EslintJson => {
                // Parsers for these formats are not yet implemented in v1.
                // The config surface is forward-compatible so users can
                // still declare them; they'll get a clear "not yet
                // implemented" diagnostic rather than silent success.
                result.diagnostics.push(Diagnostic {
                    path: orig.clone(),
                    line: 1,
                    column: 1,
                    severity: Severity::Info,
                    code: Some("TAU0002".into()),
                    message: format!(
                        "diagnostics parser '{}' is not yet implemented in this version of tau",
                        parser_name(format)
                    ),
                    source: Some("tau".into()),
                });
                result.summary.files_scanned += 1;
            }
        }

        // Surface command failure when no diagnostics were emitted for
        // this file — same logic as cargo above, scoped per-file.
        if outcome.status != 0
            && result
                .diagnostics
                .iter()
                .all(|d| d.path != *orig || d.source.as_deref() == Some("tau"))
        {
            let trimmed = outcome.stderr.trim();
            if !trimmed.is_empty() {
                let snippet: String = trimmed.lines().take(5).collect::<Vec<_>>().join("\n");
                result.skipped.push(Skipped {
                    path: orig.clone(),
                    reason: format!(
                        "provider '{}' failed (exit {}): {}",
                        name, outcome.status, snippet
                    ),
                });
            }
        }
    }
}

fn parser_name(p: ParserKind) -> &'static str {
    match p {
        ParserKind::CargoJson => "cargo-json",
        ParserKind::RustcJson => "rustc-json",
        ParserKind::RuffJson => "ruff-json",
        ParserKind::TscText => "tsc-text",
        ParserKind::EslintJson => "eslint-json",
    }
}

fn timeout_diag(path: &str, secs: u64) -> Diagnostic {
    Diagnostic {
        path: path.into(),
        line: 1,
        column: 1,
        severity: Severity::Error,
        code: Some("TAU0001".into()),
        message: format!("diagnostics provider timed out after {}s", secs),
        source: Some("tau".into()),
    }
}

// ---------------------------------------------------------------------------
// Cargo / rustc JSON parser
// ---------------------------------------------------------------------------

/// Parse cargo's JSON stream and return `Diagnostic` entries with
/// `path` set to whatever cargo emitted (relative or absolute — caller
/// canonicalises before matching).
fn parse_cargo_json(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // cargo wraps each diagnostic in `{ "reason": "compiler-message", "message": {...} }`.
        // rustc emits the `message` shape directly. Detect both.
        let msg = if v.get("reason").and_then(|r| r.as_str()) == Some("compiler-message") {
            match v.get("message") {
                Some(m) => m,
                None => continue,
            }
        } else if v.get("spans").is_some() && v.get("level").is_some() {
            &v
        } else {
            continue;
        };

        extend_from_message(msg, &mut out);
    }
    out
}

fn extend_from_message(msg: &serde_json::Value, out: &mut Vec<Diagnostic>) {
    let level = msg.get("level").and_then(|l| l.as_str()).unwrap_or("");
    let severity = match level {
        "error" | "error: internal compiler error" => Severity::Error,
        "warning" => Severity::Warning,
        "note" | "help" => Severity::Info,
        // ICEs and unknown levels: keep as error so they're not lost.
        "" => return,
        _ => Severity::Info,
    };

    let message_text = msg
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let code = msg
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());

    let spans = msg.get("spans").and_then(|s| s.as_array());
    let Some(spans) = spans else {
        return;
    };

    for span in spans {
        let is_primary = span
            .get("is_primary")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if !is_primary {
            continue;
        }
        let file = span.get("file_name").and_then(|f| f.as_str()).unwrap_or("");
        let line = span.get("line_start").and_then(|l| l.as_u64()).unwrap_or(1) as u32;
        let column = span
            .get("column_start")
            .and_then(|c| c.as_u64())
            .unwrap_or(1) as u32;

        out.push(Diagnostic {
            path: file.to_string(),
            line,
            column,
            severity,
            code: code.clone(),
            message: message_text.clone(),
            source: Some("rustc".into()),
        });
    }
}

// ---------------------------------------------------------------------------
// Cargo workspace detection
// ---------------------------------------------------------------------------

/// Walk up from `file` to find the nearest ancestor `Cargo.toml`. If that
/// crate is part of a workspace, return the workspace root instead.
fn find_cargo_workspace_root(file: &Path) -> Option<PathBuf> {
    let start = file.parent().unwrap_or(file);
    let mut nearest: Option<PathBuf> = None;
    let mut workspace: Option<PathBuf> = None;

    for ancestor in start.ancestors() {
        let candidate = ancestor.join("Cargo.toml");
        if !candidate.is_file() {
            continue;
        }
        if nearest.is_none() {
            nearest = Some(ancestor.to_path_buf());
        }
        // Read and look for `[workspace]`. Cheap parse — we don't need
        // the full toml structure.
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            if has_workspace_section(&text) {
                workspace = Some(ancestor.to_path_buf());
                break;
            }
        }
    }

    workspace.or(nearest)
}

fn has_workspace_section(text: &str) -> bool {
    // Look for a top-level `[workspace]` table header — ignore lines
    // that are inside strings or comments by doing a simple line-based
    // scan.
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        if line == "[workspace]" || line.starts_with("[workspace.") {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Subprocess plumbing (no shared state with bash.rs — copies the small
// setsid + close-fds snippet so a stuck provider doesn't leak descendants)
// ---------------------------------------------------------------------------

struct CmdOutcome {
    stdout: String,
    stderr: String,
    status: i32,
    timed_out: bool,
    cancelled: bool,
}

fn spawn_and_wait(
    mut cmd: std::process::Command,
    timeout_secs: u64,
    cancel: &CancelToken,
) -> std::io::Result<CmdOutcome> {
    // Run in a new session so we can `killpg` everything on
    // timeout/cancel and not just the direct child.
    //
    // Note: we deliberately do NOT call `close_fds_from_3()` here
    // (unlike `bash::spawn_child`). Rust's `Command::spawn`
    // implementation uses an internal CLOEXEC pipe at a fd ≥ 3 to
    // surface `exec(2)` failures back to the parent; closing it would
    // mask ENOENT-from-exec for non-existent linter binaries and
    // surface it as a child panic instead. Diagnostics providers are
    // short-lived and well-behaved (cargo, ruff, tsc), so the
    // pipe-write-end leak that motivates close_fds_from_3 in the bash
    // tool isn't a concern here.
    unsafe {
        cmd.pre_exec(|| {
            setsid().map_err(std::io::Error::other)?;
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let pgid = child.id() as i32;

    let killed = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));

    // Watchdog thread: kill on timeout.
    {
        let killed = killed.clone();
        let done = done.clone();
        let timed_out = timed_out.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(timeout_secs);
            loop {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline - now;
                std::thread::sleep(remaining.min(Duration::from_millis(100)));
            }
            if done.load(Ordering::Relaxed) {
                return;
            }
            timed_out.store(true, Ordering::Relaxed);
            killed.store(true, Ordering::Relaxed);
            let _ = signal::killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        });
    }

    // Cancel watcher thread.
    {
        let killed = killed.clone();
        let done = done.clone();
        let cancelled = cancelled.clone();
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            loop {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                if cancel.is_cancelled() {
                    cancelled.store(true, Ordering::Relaxed);
                    killed.store(true, Ordering::Relaxed);
                    let _ = signal::killpg(Pid::from_raw(pgid), Signal::SIGKILL);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }

    let stdout_handle = child
        .stdout
        .take()
        .map(|mut s| std::thread::spawn(move || read_to_end(&mut s)));
    let stderr_handle = child
        .stderr
        .take()
        .map(|mut s| std::thread::spawn(move || read_to_end(&mut s)));

    let status = child.wait()?;
    done.store(true, Ordering::Relaxed);
    killed.store(true, Ordering::Relaxed);

    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    Ok(CmdOutcome {
        stdout,
        stderr,
        status: status.code().unwrap_or(-1),
        timed_out: timed_out.load(Ordering::Relaxed),
        cancelled: cancelled.load(Ordering::Relaxed),
    })
}

fn read_to_end<R: std::io::Read>(r: &mut R) -> String {
    let mut buf = String::new();
    let _ = r.read_to_string(&mut buf);
    buf
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::Instant;
    use tempfile::TempDir;

    fn cancel() -> CancelToken {
        CancelToken::new()
    }

    fn cwd_str(dir: &TempDir) -> String {
        dir.path().to_string_lossy().to_string()
    }

    /// Decode the JSON body of a successful ToolOutput.
    fn decode(out: &ToolOutput) -> serde_json::Value {
        let text = match out.content.first() {
            Some(tau_agent_plugin::ToolResultContent::Text(t)) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        serde_json::from_str(&text).expect("body must be JSON")
    }

    fn cargo_available() -> bool {
        std::process::Command::new("cargo")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // --- pure parser tests --------------------------------------------------

    #[test]
    fn parse_cargo_compiler_message_extracts_primary_span() {
        let stdout = r#"{"reason":"compiler-message","package_id":"x","manifest_path":"/x/Cargo.toml","target":{},"message":{"message":"mismatched types","code":{"code":"E0308","explanation":null},"level":"error","spans":[{"file_name":"src/lib.rs","byte_start":10,"byte_end":11,"line_start":3,"line_end":3,"column_start":5,"column_end":6,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null},{"file_name":"src/lib.rs","line_start":1,"column_start":1,"is_primary":false}],"children":[],"rendered":"..."}}
{"reason":"compiler-message","message":{"message":"unused","code":{"code":"unused_imports","explanation":null},"level":"warning","spans":[{"file_name":"src/main.rs","line_start":7,"column_start":2,"is_primary":true}],"children":[]}}
{"reason":"compiler-message","message":{"message":"hint","code":null,"level":"note","spans":[{"file_name":"src/main.rs","line_start":7,"column_start":2,"is_primary":true}],"children":[]}}
{"reason":"build-finished","success":false}
"#;
        let diags = parse_cargo_json(stdout);
        assert_eq!(diags.len(), 3, "got: {:?}", diags);

        let err = &diags[0];
        assert_eq!(err.severity, Severity::Error);
        assert_eq!(err.code.as_deref(), Some("E0308"));
        assert_eq!(err.path, "src/lib.rs");
        assert_eq!(err.line, 3);
        assert_eq!(err.column, 5);
        assert_eq!(err.message, "mismatched types");
        assert_eq!(err.source.as_deref(), Some("rustc"));

        let warn = &diags[1];
        assert_eq!(warn.severity, Severity::Warning);
        assert_eq!(warn.code.as_deref(), Some("unused_imports"));

        let info = &diags[2];
        assert_eq!(info.severity, Severity::Info);
    }

    #[test]
    fn parse_cargo_json_skips_non_primary_and_non_compiler_lines() {
        let stdout = r#"not json
{"reason":"build-script-executed","linked_libs":[]}
{"reason":"compiler-message","message":{"message":"x","code":null,"level":"warning","spans":[{"file_name":"a.rs","line_start":1,"column_start":1,"is_primary":false}],"children":[]}}
"#;
        let diags = parse_cargo_json(stdout);
        assert!(diags.is_empty(), "got: {:?}", diags);
    }

    #[test]
    fn workspace_section_detection() {
        assert!(has_workspace_section("[workspace]\nmembers = []\n"));
        assert!(has_workspace_section(
            "# foo\n[workspace.dependencies]\nfoo=1\n"
        ));
        assert!(!has_workspace_section("[package]\nname = \"x\"\n"));
        assert!(!has_workspace_section("# [workspace]\n"));
    }

    // --- end-to-end: missing files, unknown ext ----------------------------

    #[test]
    fn missing_file_reports_error_not_panic() {
        let dir = TempDir::new().unwrap();
        let out = execute(
            json!({"paths": ["does_not_exist.rs"]}),
            &cwd_str(&dir),
            &cancel(),
        );
        let body = decode(&out);
        assert!(!out.is_error);
        let skipped = body
            .get("skipped")
            .and_then(|v| v.as_array())
            .expect("skipped array");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["path"], "does_not_exist.rs");
        assert_eq!(skipped[0]["reason"], "file not found");
    }

    #[test]
    fn unknown_extension_is_skipped_gracefully() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("notes.md"), "hi").unwrap();
        let out = execute(json!({"paths": ["notes.md"]}), &cwd_str(&dir), &cancel());
        let body = decode(&out);
        assert!(!out.is_error);
        let skipped = body
            .get("skipped")
            .and_then(|v| v.as_array())
            .expect("skipped");
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0]["reason"]
                .as_str()
                .unwrap()
                .contains("no diagnostics provider for .md"),
            "got: {}",
            skipped[0]["reason"]
        );
    }

    #[test]
    fn empty_paths_is_an_error() {
        let dir = TempDir::new().unwrap();
        let out = execute(json!({"paths": []}), &cwd_str(&dir), &cancel());
        assert!(out.is_error);
    }

    #[test]
    fn too_many_paths_rejected() {
        let dir = TempDir::new().unwrap();
        let many: Vec<String> = (0..(MAX_PATHS + 1)).map(|i| format!("f{}.rs", i)).collect();
        let out = execute(json!({"paths": many}), &cwd_str(&dir), &cancel());
        assert!(out.is_error);
    }

    // --- end-to-end: command provider error path ---------------------------

    #[test]
    fn nonexistent_command_provider_reports_skipped() {
        let dir = TempDir::new().unwrap();
        // Set up a project-tier diagnostics.toml that maps `.fake` to a
        // command which does not exist.
        let tau_dir = dir.path().join(".tau");
        fs::create_dir_all(&tau_dir).unwrap();
        fs::write(
            tau_dir.join("diagnostics.toml"),
            r#"[[providers]]
extensions = ["fake"]
cmd = ["/this/program/does/not/exist", "{file}"]
format = "cargo-json"
timeout_secs = 5
name = "fake-linter"
"#,
        )
        .unwrap();

        // And a target file to scan.
        let target = dir.path().join("hello.fake");
        fs::write(&target, "x").unwrap();

        let out = execute(json!({"paths": ["hello.fake"]}), &cwd_str(&dir), &cancel());
        let body = decode(&out);
        let skipped = body
            .get("skipped")
            .and_then(|v| v.as_array())
            .expect("skipped");
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0]["reason"]
                .as_str()
                .unwrap()
                .contains("not available"),
            "got: {}",
            skipped[0]["reason"]
        );
    }

    #[test]
    fn cancel_token_aborts_long_scan() {
        let dir = TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        fs::create_dir_all(&tau_dir).unwrap();
        fs::write(
            tau_dir.join("diagnostics.toml"),
            r#"[[providers]]
extensions = ["slow"]
cmd = ["sh", "-c", "sleep 30 # {file}"]
format = "cargo-json"
timeout_secs = 60
name = "slow"
"#,
        )
        .unwrap();
        let target = dir.path().join("a.slow");
        fs::write(&target, "x").unwrap();

        let cancel = CancelToken::new();
        let cancel_for_thread = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel_for_thread.cancel();
        });

        let start = Instant::now();
        let out = execute(json!({"paths": ["a.slow"]}), &cwd_str(&dir), &cancel);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(3), "elapsed: {:?}", elapsed);

        let body = decode(&out);
        let skipped = body
            .get("skipped")
            .and_then(|v| v.as_array())
            .expect("skipped");
        assert!(
            skipped.iter().any(|s| s["reason"] == "scan cancelled"),
            "got: {:?}",
            skipped
        );
    }

    // --- end-to-end: real cargo (gated) ------------------------------------

    fn write_cargo_crate(dir: &Path, lib_rs: &str) {
        fs::write(
            dir.join("Cargo.toml"),
            r#"[package]
name = "diag_scan_test"
version = "0.0.0"
edition = "2021"

[lib]
path = "src/lib.rs"

[workspace]
"#,
        )
        .unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), lib_rs).unwrap();
    }

    #[test]
    fn clean_rust_file_returns_empty_diagnostics() {
        if !cargo_available() {
            eprintln!("skipping: cargo not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        write_cargo_crate(dir.path(), "pub fn add(a: i32, b: i32) -> i32 { a + b }\n");

        let out = execute(json!({"paths": ["src/lib.rs"]}), &cwd_str(&dir), &cancel());
        let body = decode(&out);
        let summary = body.get("summary").expect("summary");
        assert_eq!(summary["errors"], 0, "body: {}", body);
        // Some warnings (e.g. unused) shouldn't occur for this trivial code,
        // but allow them — only assert errors == 0.
    }

    #[test]
    fn broken_rust_file_returns_structured_error() {
        if !cargo_available() {
            eprintln!("skipping: cargo not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        // Type-mismatch: pass &str where i32 is expected.
        write_cargo_crate(
            dir.path(),
            "pub fn bad() -> i32 { let s: i32 = \"hi\"; s }\n",
        );

        let out = execute(json!({"paths": ["src/lib.rs"]}), &cwd_str(&dir), &cancel());
        let body = decode(&out);
        let diagnostics = body
            .get("diagnostics")
            .and_then(|v| v.as_array())
            .expect("diagnostics");
        assert!(
            diagnostics.iter().any(|d| d["severity"] == "error"),
            "expected at least one error, got: {}",
            body
        );
        let err = diagnostics
            .iter()
            .find(|d| d["severity"] == "error")
            .unwrap();
        assert!(err["line"].as_u64().unwrap() >= 1);
        assert!(err["column"].as_u64().unwrap() >= 1);
        assert_eq!(err["source"], "rustc");
        assert!(err["code"].is_string(), "expected error code, got: {}", err);
    }

    #[test]
    fn out_of_workspace_rust_file_skipped_with_reason() {
        if !cargo_available() {
            eprintln!("skipping: cargo not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        // Bail if some ancestor of our tempdir already has a Cargo.toml
        // — we can't simulate "loose file with no workspace ancestor"
        // in that environment without polluting global state. (Common
        // on dev machines: a stray /tmp/Cargo.toml.)
        if find_cargo_workspace_root(dir.path()).is_some() {
            eprintln!(
                "skipping: ancestor of {} contains Cargo.toml",
                dir.path().display()
            );
            return;
        }
        // A bare .rs file with no Cargo.toml ancestor.
        let f = dir.path().join("loose.rs");
        fs::write(&f, "fn main() {}\n").unwrap();

        let out = execute(json!({"paths": ["loose.rs"]}), &cwd_str(&dir), &cancel());
        let body = decode(&out);
        let skipped = body
            .get("skipped")
            .and_then(|v| v.as_array())
            .expect("skipped");
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0]["reason"]
                .as_str()
                .unwrap()
                .contains("no Cargo.toml ancestor"),
            "got: {}",
            skipped[0]["reason"]
        );
    }

    #[test]
    fn mixed_paths_partition_correctly() {
        if !cargo_available() {
            eprintln!("skipping: cargo not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        write_cargo_crate(dir.path(), "pub fn ok() {}\n");
        fs::write(dir.path().join("README.md"), "readme").unwrap();

        let out = execute(
            json!({"paths": ["src/lib.rs", "README.md"]}),
            &cwd_str(&dir),
            &cancel(),
        );
        let body = decode(&out);
        let skipped = body
            .get("skipped")
            .and_then(|v| v.as_array())
            .expect("skipped");
        assert!(
            skipped.iter().any(|s| s["path"] == "README.md"),
            "expected README.md in skipped, got: {:?}",
            skipped
        );
        let summary = body.get("summary").expect("summary");
        assert_eq!(summary["errors"], 0);
        assert!(summary["files_skipped"].as_u64().unwrap() >= 1);
    }
}
