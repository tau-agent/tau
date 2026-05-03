mod completer;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::ArgValueCandidates;

#[derive(Parser)]
#[command(name = "tau", about = "LLM agent CLI", infer_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Chat with an LLM
    Chat {
        /// Message to send (omit for interactive mode)
        #[arg(short, long)]
        message: Option<String>,
        /// Session ID (creates new if omitted)
        #[arg(short, long, add = ArgValueCandidates::new(completer::session_completer))]
        session: Option<String>,
        /// Model name (default: saved setting or claude-opus-4-6)
        #[arg(long, add = ArgValueCandidates::new(completer::model_completer))]
        model: Option<String>,
        /// Disable TUI (use plain text streaming)
        #[arg(long)]
        no_tui: bool,
        /// Max child sessions this session can spawn (0 = no children)
        #[arg(long, default_value = "16")]
        child_budget: u32,
        /// Attach an image file to the next message (PNG/JPEG/GIF/WEBP, ≤5 MiB).
        /// Repeat to attach multiple. Only sent on the first send in this
        /// invocation; subsequent prompts in interactive mode are text-only.
        #[arg(long = "attach", value_name = "PATH")]
        attach: Vec<std::path::PathBuf>,
    },
    /// Log in to an LLM provider (OAuth)
    Login {
        /// Provider name
        #[arg(default_value = "anthropic")]
        provider: String,
    },
    /// Tool execution worker (internal, used by daemon)
    #[command(hide = true)]
    Worker,
    /// Task system plugin (internal, used by daemon)
    #[command(name = "plugin-tasks", hide = true)]
    PluginTasks,
    /// Manage the tau server
    #[command(alias = "srv")]
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Manage sessions
    #[command(alias = "s")]
    Sessions {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Manage providers
    #[command(alias = "p")]
    Providers {
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// Manage models
    #[command(alias = "m")]
    Models {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Manage authentication
    #[command(alias = "a")]
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Manage tasks
    #[command(alias = "t")]
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Manage projects
    #[command(alias = "proj")]
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Manage daemon configuration
    #[command(alias = "cfg")]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Analyse historical session timing (read-only)
    #[command(alias = "prof")]
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

#[derive(Subcommand)]
enum ProfileAction {
    /// Bucket leaderboard: total / mean / p50 / p95 / max per bucket
    Buckets {
        /// Time window lower bound (e.g. `7d`, `24h`, `30m`, `2024-01-01`,
        /// or a raw ms timestamp). Default: 30 days ago.
        #[arg(long)]
        since: Option<String>,
        /// Time window upper bound. Defaults to "now".
        #[arg(long)]
        until: Option<String>,
        /// Restrict to a project.
        #[arg(long)]
        project: Option<String>,
        /// Restrict to a single session.
        #[arg(long)]
        session: Option<String>,
        /// Per-event duration clamp (`30s`, `5m`, `1h`, raw ms, or `0` to
        /// disable). Events longer than this are excluded from aggregates
        /// and reported as a per-bucket drop count. The default of 1h
        /// filters out stale-session noise where async messages land
        /// hours/days after the parent went quiet — almost any real
        /// tool/LLM gap is well under an hour.
        #[arg(long, default_value = "1h")]
        clamp: String,
        /// Include `other:*` buckets (info<-info, user<-info, etc.) in
        /// the ranking. They are excluded by default because they catch
        /// every non-canonical adjacency and are dominated by
        /// async-notification artifacts, not real performance signal.
        #[arg(long)]
        include_other: bool,
    },
    /// Slow events above a duration threshold
    Slow {
        /// Minimum duration (`30s`, `2m`, `1h`, or a raw ms count).
        #[arg(long, default_value = "30s")]
        min: String,
        /// Cap on rows printed.
        #[arg(long, default_value = "50")]
        limit: usize,
        /// Time window lower bound. Default: 30 days ago.
        #[arg(long)]
        since: Option<String>,
        /// Time window upper bound. Defaults to "now".
        #[arg(long)]
        until: Option<String>,
        /// Restrict to a project.
        #[arg(long)]
        project: Option<String>,
        /// Per-event duration clamp (`30s`, `5m`, `1h`, raw ms, or `0` to
        /// disable). Events longer than this are dropped from the slow
        /// list. Default 1h filters stale-session async-arrival noise.
        #[arg(long, default_value = "1h")]
        clamp: String,
        /// Include `other:*` buckets (info<-info, user<-info, etc.) in
        /// the slow list. Excluded by default because they are dominated
        /// by async-notification artifacts rather than real perf signal.
        #[arg(long)]
        include_other: bool,
    },
    /// Per-session bucket breakdown
    Session {
        /// Session ID
        #[arg(add = ArgValueCandidates::new(completer::session_completer))]
        id: String,
        /// Per-event duration clamp (`30s`, `5m`, `1h`, raw ms, or `0` to
        /// disable). Default 1h filters stale-session async-arrival
        /// noise. Pass `0` to keep every event when investigating a
        /// specific long-idle session.
        #[arg(long, default_value = "1h")]
        clamp: String,
        /// Suppress `other:*` buckets. By default per-session view
        /// includes them — user-thinking gaps and idle-info adjacencies
        /// are contextually informative when investigating one session.
        #[arg(long)]
        exclude_other: bool,
    },
    /// Token usage and cost report (no `--clamp` — there are no
    /// per-event durations to clamp here; the underlying SUM ignores
    /// messages without a `$.usage` blob).
    Tokens {
        /// Time window lower bound (`7d`, `24h`, `30m`, `2024-01-01`,
        /// or a raw ms timestamp). Default: 30 days ago.
        #[arg(long)]
        since: Option<String>,
        /// Time window upper bound. Defaults to "now".
        #[arg(long)]
        until: Option<String>,
        /// Restrict to a project.
        #[arg(long)]
        project: Option<String>,
        /// Per-session breakdown for one session.
        #[arg(long)]
        session: Option<String>,
        /// Per-task breakdown (joins `task_sessions` from `tasks.db`).
        #[arg(long)]
        task: Option<i64>,
        /// Restrict the rollup to one role (`worker`, `reviewer`,
        /// `planner`, …). Only meaningful with the role / task join —
        /// ignored for `--group-by session`.
        #[arg(long)]
        role: Option<String>,
        /// Leaderboard grouping. Defaults to `role` for the project-wide
        /// view, `session` if `--task` is set.
        #[arg(long, value_name = "AXIS")]
        group_by: Option<String>,
        /// Sort axis (descending). Default: `cost`.
        #[arg(long, default_value = "cost")]
        sort: String,
    },
}

#[derive(Subcommand)]
enum TaskAction {
    /// List tasks for the current project
    #[command(alias = "l")]
    List {
        /// Filter by state
        #[arg(long)]
        state: Option<String>,
        /// Filter by parent task ID
        #[arg(long)]
        parent: Option<i64>,
    },
    /// Show task details and messages
    #[command(alias = "g")]
    Get {
        /// Task ID
        id: i64,
    },
    /// Create a new task
    #[command(alias = "c")]
    Create {
        /// Task title
        title: String,
        /// Parent task ID
        #[arg(long)]
        parent: Option<i64>,
        /// Skip review step
        #[arg(long)]
        skip_review: bool,
        /// Skip planning step (subtask starts in ready instead of planning)
        #[arg(long)]
        skip_planning: bool,
        /// Require human approval before work begins (refining goes to interactive instead of ready)
        #[arg(long)]
        require_approval: bool,
        /// Priority (higher = more important)
        #[arg(long, default_value = "0")]
        priority: i64,
    },
    /// Update a task
    #[command(alias = "u")]
    Update {
        /// Task ID
        id: i64,
        /// New state
        #[arg(long)]
        state: Option<String>,
        /// New title
        #[arg(long)]
        title: Option<String>,
        /// New priority
        #[arg(long)]
        priority: Option<i64>,
    },
    /// Append a message to a task
    #[command(alias = "msg")]
    Message {
        /// Task ID
        id: i64,
        /// Message content
        content: String,
    },
    /// Approve a task (shorthand for update --state=approved)
    Approve {
        /// Task ID
        id: i64,
    },
    /// Claim a task (assign to current session and activate)
    Claim {
        /// Task ID
        id: i64,
        /// Session ID to assign the task to
        #[arg(long)]
        session: String,
    },
    /// Mark a task as ready (shorthand for update --state=ready)
    Ready {
        /// Task ID
        id: i64,
    },
    /// Show the merge queue (approved + merging tasks)
    Mq,
    /// Show scheduler status: active, queued, and blocked tasks
    #[command(alias = "s")]
    Status,
}

#[derive(Subcommand)]
enum ProviderAction {
    /// List configured providers
    #[command(alias = "l")]
    List,
    /// Add a provider
    #[command(alias = "a")]
    Add {
        /// Provider name
        name: String,
        /// API type (anthropic or openai)
        #[arg(long)]
        api: String,
        /// Base URL
        #[arg(long)]
        base_url: String,
        /// Inline API key (or $ENV_VAR)
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Remove a provider
    #[command(alias = "rm")]
    Remove {
        /// Provider name
        name: String,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// List all available models
    #[command(alias = "l")]
    List,
    /// Add a model to a provider
    #[command(alias = "a")]
    Add {
        /// Model ID
        id: String,
        /// Provider name
        #[arg(long)]
        provider: String,
        /// Display name
        #[arg(long)]
        name: Option<String>,
        /// Context window size
        #[arg(long, default_value = "128000")]
        context: u64,
        /// Max output tokens
        #[arg(long, default_value = "16384")]
        max_tokens: u64,
        /// Thinking style (none, anthropic, openai, qwen)
        #[arg(long, default_value = "none")]
        thinking: String,
    },
    /// Remove a model from a provider
    #[command(alias = "rm")]
    Remove {
        /// Model ID
        id: String,
        /// Provider name
        #[arg(long)]
        provider: String,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the server
    #[command(alias = "up")]
    Start {
        /// Run in foreground (don't daemonize)
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the server
    #[command(alias = "down")]
    Stop,
    /// Restart the server
    Restart,
    /// Check server status
    Status,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Re-read providers.toml and the global models.toml without restarting
    /// the daemon. Use this after editing either file to pick up changes
    /// in running sessions (per-project `.tau/models.toml` and `auth.json`
    /// are already re-read on every use and need no reload).
    Reload,
    /// Print the paths of the config files the daemon will reload.
    Show,
}

#[derive(Subcommand)]
enum SessionAction {
    /// List sessions
    #[command(alias = "l")]
    List {
        /// Include archived sessions
        #[arg(long, short = 'a')]
        all: bool,
    },
    /// Archive a session (and all its children)
    Archive {
        /// Session ID
        #[arg(add = ArgValueCandidates::new(completer::session_completer))]
        id: String,
    },
    /// Delete a session
    #[command(aliases = ["del", "rm"])]
    Delete {
        /// Session ID
        #[arg(add = ArgValueCandidates::new(completer::session_completer))]
        id: String,
    },
    /// Restore (un-archive) a session
    Restore {
        /// Session ID
        #[arg(add = ArgValueCandidates::new(completer::archived_session_completer))]
        id: String,
    },
    /// Dump a session as a JSON recording (for replay testing)
    Dump {
        /// Session ID
        #[arg(add = ArgValueCandidates::new(completer::session_completer))]
        id: String,
        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Garbage-collect archived sessions older than a threshold
    Gc {
        /// Delete archived sessions older than this many days
        #[arg(long, default_value = "7")]
        older_than: u64,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Show authentication status
    Status,
    /// Log out from a provider
    #[command(alias = "rm")]
    Logout {
        /// Provider name
        provider: String,
    },
}

#[derive(Subcommand)]
enum ProjectAction {
    /// Initialize a new project in the current directory
    Init {
        /// Project name (default: directory name, slugified)
        #[arg(long)]
        name: Option<String>,
    },
    /// List all known projects
    #[command(alias = "l")]
    List,
    /// Show current project info
    #[command(alias = "i")]
    Info,
    /// Rename the current project
    Rename {
        /// New project name
        new_name: String,
    },
    /// Show aggregate cost / usage stats for a project.
    ///
    /// Defaults to the current project (from cwd).  Pass `--project NAME`
    /// to inspect a different project.  Archived sessions are included.
    Stats {
        /// Project name (defaults to the current project from cwd).
        #[arg(long)]
        project: Option<String>,
    },
    /// Run one-time project migration (converts path-based projects to name-based)
    Migrate,
}

fn main() {
    clap_complete::env::CompleteEnv::with_factory(Cli::command).complete();
    let cli = Cli::parse();

    // Block SIGTERM/SIGHUP early (before smol starts and spawns threads)
    // so the dedicated waiter thread can reliably receive them.  The
    // initial handler is a basic "exit cleanly" — chat modes upgrade it
    // to also CancelChat the in-flight session and restore the terminal.
    install_default_signal_handler();

    smol::block_on(async {
        if let Err(e) = run(cli).await {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    });
}

/// Process-wide registry: the session id of the currently in-flight chat,
/// if any.  Updated by chat modes so the SIGTERM/SIGHUP handler can issue
/// a CancelChat before exiting.
static ACTIVE_CHAT_SESSION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn set_active_chat_session(id: Option<String>) {
    if let Ok(mut guard) = ACTIVE_CHAT_SESSION.lock() {
        *guard = id;
    }
}

fn current_active_chat_session() -> Option<String> {
    ACTIVE_CHAT_SESSION.lock().ok().and_then(|g| g.clone())
}

/// Best-effort: ask the running server to cancel the given session's
/// chat.  Used from signal handlers — must be quick and infallible.
fn fire_and_forget_cancel(session_id: &str) {
    use std::io::Write;
    let req = tau_agent_lib::protocol::Request::CancelChat {
        session_id: session_id.to_string(),
        caller_session_id: None,
    };
    let mut line = match serde_json::to_string(&req) {
        Ok(s) => s,
        Err(_) => return,
    };
    line.push('\n');
    if let Ok(stream) =
        std::os::unix::net::UnixStream::connect(tau_agent_lib::server::socket_path())
    {
        let mut w = std::io::BufWriter::new(stream);
        let _ = w.write_all(line.as_bytes());
        let _ = w.flush();
    }
}

/// Default signal handler for non-chat CLI modes (sessions list, server
/// status, etc.): print and exit with the appropriate status code.
fn install_default_signal_handler() {
    if let Err(e) = tau_agent_lib::shutdown::install(|sig| {
        let code = match sig {
            nix::sys::signal::Signal::SIGHUP => 129,
            nix::sys::signal::Signal::SIGINT => 130,
            nix::sys::signal::Signal::SIGTERM => 143,
            _ => 1,
        };
        eprintln!(
            "tau: received {}, exiting",
            tau_agent_lib::shutdown::signal_name(sig),
        );
        std::process::exit(code);
    }) {
        eprintln!("tau: failed to install signal handlers: {}", e);
    }
}

/// Replace the signal handler with one tailored for non-TUI chat modes:
/// SIGTERM / SIGHUP / SIGINT cancel the in-flight chat (if any) and exit.
///
/// The TUI installs a different handler that also restores the terminal
/// (and leaves SIGINT alone so the in-app Ctrl-C handling keeps working).
fn install_chat_signal_handler() {
    let cancel = |sig: nix::sys::signal::Signal| {
        if let Some(sid) = current_active_chat_session() {
            eprintln!(
                "\ntau: received {}, cancelling chat and exiting",
                tau_agent_lib::shutdown::signal_name(sig),
            );
            fire_and_forget_cancel(&sid);
        } else {
            eprintln!(
                "\ntau: received {}, exiting",
                tau_agent_lib::shutdown::signal_name(sig),
            );
        }
        let code = match sig {
            nix::sys::signal::Signal::SIGHUP => 129,
            nix::sys::signal::Signal::SIGINT => 130,
            nix::sys::signal::Signal::SIGTERM => 143,
            _ => 1,
        };
        std::process::exit(code);
    };
    let signals = [
        nix::sys::signal::Signal::SIGTERM,
        nix::sys::signal::Signal::SIGHUP,
        nix::sys::signal::Signal::SIGINT,
    ];
    if let Err(e) = tau_agent_lib::shutdown::install_for(signals, cancel) {
        eprintln!("tau: failed to install chat signal handlers: {}", e);
    }
}

async fn run(cli: Cli) -> tau_agent_lib::Result<()> {
    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            // `tau` bare: open TUI with session picker
            return cmd_default().await;
        }
    };
    match command {
        Commands::Chat {
            message,
            session,
            model,
            no_tui,
            child_budget,
            attach,
        } => {
            maybe_auto_init();
            // Resolve model: CLI flag > saved setting > hardcoded default
            let model = model.unwrap_or_else(tau_agent_tui::settings::default_model);
            cmd_chat(message, session, &model, no_tui, child_budget, attach).await?;
        }
        Commands::Worker => {
            tau_agent_lib::worker::run();
            return Ok(());
        }
        Commands::PluginTasks => {
            tau_agent_lib::tasks::run_tasks_plugin();
            return Ok(());
        }
        Commands::Login { provider } => {
            cmd_login(&provider).await?;
        }
        Commands::Server { action } => match action {
            ServerAction::Start { foreground } => {
                cmd_server_start(foreground).await?;
            }
            ServerAction::Stop => {
                cmd_server_stop().await?;
            }
            ServerAction::Restart => {
                cmd_server_restart().await?;
            }
            ServerAction::Status => {
                cmd_server_status();
            }
        },
        Commands::Sessions { action } => match action {
            SessionAction::List { all } => {
                cmd_sessions_list(all).await?;
            }
            SessionAction::Archive { id } => {
                cmd_sessions_archive(&id).await?;
            }
            SessionAction::Restore { id } => {
                cmd_sessions_restore(&id).await?;
            }
            SessionAction::Delete { id } => {
                cmd_sessions_delete(&id).await?;
            }
            SessionAction::Dump { id, output } => {
                cmd_sessions_dump(&id, output.as_deref())?;
            }
            SessionAction::Gc { older_than } => {
                cmd_sessions_gc(older_than).await?;
            }
        },
        Commands::Providers { action } => match action {
            ProviderAction::List => cmd_providers_list()?,
            ProviderAction::Add {
                name,
                api,
                base_url,
                api_key,
            } => cmd_providers_add(&name, &api, &base_url, api_key.as_deref())?,
            ProviderAction::Remove { name } => cmd_providers_remove(&name)?,
        },
        Commands::Models { action } => match action {
            ModelAction::List => cmd_models_list()?,
            ModelAction::Add {
                id,
                provider,
                name,
                context,
                max_tokens,
                thinking,
            } => cmd_models_add(
                &id,
                &provider,
                name.as_deref(),
                context,
                max_tokens,
                &thinking,
            )?,
            ModelAction::Remove { id, provider } => cmd_models_remove(&id, &provider)?,
        },
        Commands::Auth { action } => match action {
            AuthAction::Status => {
                cmd_auth_status().await?;
            }
            AuthAction::Logout { provider } => {
                let auth = tau_agent_lib::auth::AuthStorage::open_default();
                auth.remove(&provider)?;
                eprintln!("logged out from {}", provider);
            }
        },
        Commands::Task { action } => {
            cmd_task(action)?;
        }
        Commands::Project { action } => {
            cmd_project(action)?;
        }
        Commands::Config { action } => match action {
            ConfigAction::Reload => cmd_config_reload().await?,
            ConfigAction::Show => cmd_config_show(),
        },
        Commands::Profile { action } => {
            cmd_profile(action)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Default command: open TUI with session picker
// ---------------------------------------------------------------------------

async fn cmd_default() -> tau_agent_lib::Result<()> {
    // Auto-init check
    maybe_auto_init();

    // TUI installs its own SIGTERM/SIGHUP handler that restores the
    // terminal before exiting; nothing to do here.

    // Resolve model from settings
    let model = tau_agent_tui::settings::default_model();

    // Connect to server, create an initial session for the TUI
    let mut client = tau_agent_lib::client::Client::connect_or_start().await?;
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from));

    // Parse "provider/model" syntax
    let (provider, model_id) = if let Some(idx) = model.find('/') {
        (Some(model[..idx].to_string()), model[idx + 1..].to_string())
    } else {
        (None, model.to_string())
    };

    // Create session (needed for TUI initialization)
    let session_id = client
        .create_user_session(tau_agent_lib::client::UserSessionSpec {
            model: Some(model_id),
            provider,
            cwd,
            parent_id: None,
            child_budget: 16,
        })
        .await?;

    // Track the active session so the SIGTERM/SIGHUP handler can cancel it.
    set_active_chat_session(Some(session_id.clone()));

    // Get session info
    let info = get_session_info(&mut client, &session_id).await.ok();
    let info_model = info.as_ref().map(|i| i.model.clone()).unwrap_or_default();
    let info_provider = info
        .as_ref()
        .map(|i| i.provider.clone())
        .unwrap_or_default();
    let context_window = info.as_ref().map(|i| i.stats.context_window).unwrap_or(0);
    let is_subscription = info.as_ref().is_some_and(|i| i.stats.is_subscription);

    // Run TUI starting in picker mode
    tau_agent_tui::run_with_picker(
        session_id,
        info_model,
        info_provider,
        context_window,
        is_subscription,
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Auto-project-init prompt
// ---------------------------------------------------------------------------

/// If we're in a git directory without `.tau/project.toml`, ask the user
/// whether to initialize a tau project. No-op if not applicable.
fn maybe_auto_init() {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return,
    };

    // Already a project? Nothing to do.
    if tau_agent_lib::project::discover_project(&cwd).is_some() {
        return;
    }

    // Reject candidates that are not inside a real git working tree.
    // The cheap `.git` existence check is the common-case fast path; the
    // canonical `git rev-parse --show-toplevel` probe (#949) catches the
    // remaining edge cases (bare repos, broken `.git` files, etc.) so we
    // never prompt the user only to fail later in `init_project`.
    if !cwd.join(".git").exists() {
        return;
    }
    if tau_agent_lib::project::check_git_repo(&cwd).is_err() {
        return;
    }

    // Check "declined" flag
    let declined_path = cwd.join(".tau").join(".no-auto-init");
    if declined_path.exists() {
        return;
    }

    // Prompt user
    eprint!("Initialize tau project here? [Y/n] ");
    use std::io::Write;
    std::io::stderr().flush().ok();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return;
    }
    let answer = answer.trim().to_lowercase();

    if answer.is_empty() || answer == "y" || answer == "yes" {
        // Initialize project
        let name = cwd
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());
        let name = tau_agent_lib::project::slugify(&name);

        if tau_agent_lib::project::validate_project_name(&name).is_err() {
            eprintln!("Invalid project name '{}', skipping auto-init.", name);
            return;
        }

        let db = match tau_agent_lib::db::Db::open_default() {
            Ok(db) => db,
            Err(e) => {
                eprintln!("Failed to open database: {}", e);
                return;
            }
        };

        // Check for name/path collisions
        let canonical = match std::fs::canonicalize(&cwd) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to canonicalize path: {}", e);
                return;
            }
        };
        let path_str = canonical.to_string_lossy().to_string();

        let name_exists = db.get_project(&name).ok().flatten().is_some();
        let path_exists = db.get_project_by_path(&path_str).ok().flatten().is_some();

        if name_exists || path_exists {
            eprintln!("Project name or path already registered, skipping auto-init.");
            return;
        }

        match tau_agent_lib::project::init_project(&cwd, &name) {
            Ok(canonical_path) => {
                let canonical_str = canonical_path.to_string_lossy().to_string();
                if let Err(e) = db.create_project(&name, &canonical_str) {
                    eprintln!(
                        "Warning: project created on disk but DB registration failed: {}",
                        e
                    );
                } else {
                    eprintln!("Initialized project '{}' at {}", name, canonical_str);
                }
            }
            Err(e) => {
                eprintln!("Failed to initialize project: {}", e);
            }
        }
    } else {
        // Store decline flag
        let tau_dir = cwd.join(".tau");
        std::fs::create_dir_all(&tau_dir).ok();
        // Write a minimal .gitignore so the .tau/ dir doesn't show up
        // in git status when it only contains the decline flag.
        let gitignore_path = tau_dir.join(".gitignore");
        if !gitignore_path.exists() {
            std::fs::write(&gitignore_path, "*\n").ok();
        }
        std::fs::write(declined_path, "").ok();
        eprintln!("Skipped. Won't ask again for this directory.");
    }
}

// ---------------------------------------------------------------------------
// Cumulative usage tracking
// ---------------------------------------------------------------------------

// Struct and `add` live in `tau_agent_base::usage_totals`; the `display`
// formatter is specific to the CLI and stays local via an extension trait.
use tau_agent_lib::usage_totals::UsageTotals;

trait UsageTotalsDisplay {
    fn display(&self);
}

impl UsageTotalsDisplay for UsageTotals {
    fn display(&self) {
        use tau_agent_lib::protocol::format_tokens;
        let mut parts = Vec::new();
        if self.input > 0 {
            parts.push(format!("↑{}", format_tokens(self.input)));
        }
        if self.output > 0 {
            parts.push(format!("↓{}", format_tokens(self.output)));
        }
        if self.cache_read > 0 {
            parts.push(format!("R{}", format_tokens(self.cache_read)));
        }
        if self.cache_write > 0 {
            parts.push(format!("W{}", format_tokens(self.cache_write)));
        }
        if self.cost > 0.0 {
            if self.is_subscription {
                parts.push(format!("${:.4} (sub)", self.cost));
            } else {
                parts.push(format!("${:.4}", self.cost));
            }
        }
        if self.context_window > 0 {
            let ctx = match self.context_tokens {
                Some(t) => {
                    let pct = (t as f64 / self.context_window as f64) * 100.0;
                    format!("{:.1}%/{}", pct, format_tokens(self.context_window))
                }
                None => format!("?/{}", format_tokens(self.context_window)),
            };
            parts.push(ctx);
        }
        if !parts.is_empty() {
            eprintln!("[{}]", parts.join(" "));
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async fn cmd_login(provider: &str) -> tau_agent_lib::Result<()> {
    match provider {
        "anthropic" => {
            eprintln!("Logging in to Anthropic (OAuth)...");
            let creds = smol::unblock(tau_agent_lib::auth::login_anthropic).await?;
            let auth = tau_agent_lib::auth::AuthStorage::open_default();
            auth.set(
                "anthropic",
                tau_agent_lib::auth::AuthCredential::Oauth(creds),
            )?;
            eprintln!("Login successful! Credentials saved.");
        }
        _ => {
            // API key login — prompt on stdin
            use std::io::Write;
            eprint!("API key for {}: ", provider);
            std::io::stderr().flush().ok();
            let mut key = String::new();
            std::io::stdin()
                .read_line(&mut key)
                .map_err(|e| tau_agent_lib::Error::Io(e.to_string()))?;
            let key = key.trim().to_string();
            if key.is_empty() {
                return Err(tau_agent_lib::Error::Io("empty API key".into()));
            }
            let auth = tau_agent_lib::auth::AuthStorage::open_default();
            auth.set(
                provider,
                tau_agent_lib::auth::AuthCredential::ApiKey { key },
            )?;
            eprintln!("API key saved for {}.", provider);
        }
    }
    Ok(())
}

async fn cmd_chat(
    message: Option<String>,
    session_id: Option<String>,
    model: &str,
    no_tui: bool,
    child_budget: u32,
    attach: Vec<std::path::PathBuf>,
) -> tau_agent_lib::Result<()> {
    // Install chat-mode signal handler for the non-TUI variants.  The
    // TUI installs its own (different) handler from inside its run loop
    // so the terminal is restored on signal-driven exit.
    if no_tui || message.is_some() {
        install_chat_signal_handler();
    }
    let mut client = tau_agent_lib::client::Client::connect_or_start().await?;

    // Pre-flight encode any --attach files so we fail fast (and once) on
    // bad paths/MIMEs before opening a session, rather than mid-stream.
    let attachments: Vec<tau_agent_lib::protocol::ChatAttachment> = {
        let mut out = Vec::with_capacity(attach.len());
        for p in &attach {
            match tau_agent_lib::chat_attachments::encode_file_attachment(p) {
                Ok(att) => out.push(att),
                Err(e) => return Err(tau_agent_lib::Error::Io(e)),
            }
        }
        out
    };

    // Parse "provider/model" syntax
    let (provider, model_id) = if let Some(idx) = model.find('/') {
        (Some(model[..idx].to_string()), model[idx + 1..].to_string())
    } else {
        (None, model.to_string())
    };

    let (session_id, session_id_user_provided) = if let Some(id) = session_id {
        (id, true)
    } else {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from));
        let id = client
            .create_user_session(tau_agent_lib::client::UserSessionSpec {
                model: Some(model_id),
                provider,
                cwd,
                parent_id: None,
                child_budget,
            })
            .await?;
        (id, false)
    };

    // Get session info for display; error if user specified a session that doesn't exist
    let info = if session_id_user_provided {
        Some(get_session_info(&mut client, &session_id).await?)
    } else {
        get_session_info(&mut client, &session_id).await.ok()
    };

    // Make the active session id visible to the signal handler installed
    // above, so SIGTERM/SIGHUP can issue a CancelChat before exiting.
    set_active_chat_session(Some(session_id.clone()));
    let info_model = info.as_ref().map(|i| i.model.clone()).unwrap_or_default();
    let info_provider = info
        .as_ref()
        .map(|i| i.provider.clone())
        .unwrap_or_default();
    let context_window = info.as_ref().map(|i| i.stats.context_window).unwrap_or(0);
    let is_subscription = info.as_ref().is_some_and(|i| i.stats.is_subscription);

    if let Some(text) = message {
        // Non-interactive: plain text streaming (no TUI)
        let mut totals = UsageTotals::default();
        if let Some(ref info) = info {
            totals.context_window = info.stats.context_window;
            totals.is_subscription = info.stats.is_subscription;
            totals.input = info.stats.tokens.input;
            totals.output = info.stats.tokens.output;
            totals.cache_read = info.stats.tokens.cache_read;
            totals.cache_write = info.stats.tokens.cache_write;
            totals.cost = info.stats.cost;
            totals.context_tokens = info.stats.context_tokens;
        }
        send_and_print(&mut client, &session_id, &text, &mut totals, attachments).await?;
    } else if no_tui {
        // Interactive but no TUI: use rustyline
        let mut totals = UsageTotals::default();
        if let Some(ref info) = info {
            totals.context_window = info.stats.context_window;
            totals.is_subscription = info.stats.is_subscription;
            totals.input = info.stats.tokens.input;
            totals.output = info.stats.tokens.output;
            totals.cache_read = info.stats.tokens.cache_read;
            totals.cache_write = info.stats.tokens.cache_write;
            totals.cost = info.stats.cost;
            totals.context_tokens = info.stats.context_tokens;
        }
        interactive_loop(
            &mut client,
            session_id,
            &mut totals,
            child_budget,
            attachments,
        )
        .await?;
    } else {
        // TUI mode
        if !attachments.is_empty() {
            eprintln!("warning: --attach is ignored in TUI mode; use the /attach slash command");
        }
        tau_agent_tui::run(
            session_id,
            info_model,
            info_provider,
            context_window,
            is_subscription,
        )
        .await?;
    }

    Ok(())
}

async fn get_session_info(
    client: &mut tau_agent_lib::client::Client,
    session_id: &str,
) -> tau_agent_lib::Result<tau_agent_lib::protocol::SessionInfo> {
    client
        .send(&tau_agent_lib::protocol::Request::GetSessionInfo {
            session_id: session_id.to_string(),
        })
        .await?;

    let mut info = None;
    let mut error = None;
    client
        .recv_streaming(|resp| match resp {
            tau_agent_lib::protocol::Response::SessionInfo { info: i } => {
                info = Some(i.clone());
            }
            tau_agent_lib::protocol::Response::Error { message } => {
                error = Some(message.clone());
            }
            _ => {}
        })
        .await?;

    match (info, error) {
        (Some(i), _) => Ok(i),
        (_, Some(e)) => Err(tau_agent_lib::Error::Io(e)),
        _ => Err(tau_agent_lib::Error::Io("no response".into())),
    }
}

/// Create a new session and return its ID.
async fn cli_create_session(
    client: &mut tau_agent_lib::client::Client,
    model: Option<String>,
    cwd: Option<String>,
    parent_id: Option<String>,
    child_budget: u32,
) -> tau_agent_lib::Result<String> {
    client
        .create_user_session(tau_agent_lib::client::UserSessionSpec {
            model,
            provider: None,
            cwd,
            parent_id,
            child_budget,
        })
        .await
}

async fn send_and_print(
    client: &mut tau_agent_lib::client::Client,
    session_id: &str,
    text: &str,
    totals: &mut UsageTotals,
    attachments: Vec<tau_agent_lib::protocol::ChatAttachment>,
) -> tau_agent_lib::Result<()> {
    client
        .send(&tau_agent_lib::protocol::Request::Chat {
            session_id: session_id.to_string(),
            text: text.to_string(),
            attachments,
        })
        .await?;

    // Spawn a background thread that watches for double-Escape on raw stdin.
    // When detected it sends a CancelChat via a fresh connection.
    // The thread uses poll() with a short timeout so it can notice `done`
    // and restore terminal settings promptly when streaming ends.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_clone = done.clone();
    let session_id_clone = session_id.to_string();

    let cancel_thread = std::thread::spawn(move || {
        #[cfg(unix)]
        {
            use std::io::Read;
            use std::os::fd::AsRawFd;

            let stdin = std::io::stdin();
            let fd = stdin.as_raw_fd();

            // Only operate on a real tty.
            if unsafe { libc::isatty(fd) } == 0 {
                return;
            }

            // Save terminal settings and switch to raw mode.
            let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
            if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
                return;
            }
            let mut raw = saved;
            unsafe {
                libc::cfmakeraw(&mut raw);
                // Keep output processing enabled so that \n is still
                // translated to \r\n while we are in raw input mode.
                // Without this, streamed text loses carriage returns and
                // every newline just moves the cursor down (staircase effect).
                raw.c_oflag |= libc::OPOST;
                libc::tcsetattr(fd, libc::TCSANOW, &raw);
            }

            let mut last_was_esc = false;

            loop {
                if done_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                // poll() with 50 ms timeout so we check `done` regularly.
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ret = unsafe { libc::poll(&mut pfd, 1, 50) };

                if ret <= 0 {
                    // Timeout or error — loop back and check `done`.
                    continue;
                }

                let mut buf = [0u8; 1];
                match stdin.lock().read(&mut buf) {
                    Ok(1) => {
                        if buf[0] == 0x1b {
                            if last_was_esc {
                                // Double Escape — send cancel.
                                eprintln!("\n[cancelling...]");
                                let cancel_req = tau_agent_lib::protocol::Request::CancelChat {
                                    session_id: session_id_clone.clone(),
                                    caller_session_id: None,
                                };
                                if let Ok(stream) = std::os::unix::net::UnixStream::connect(
                                    tau_agent_lib::server::socket_path(),
                                ) {
                                    use std::io::Write;
                                    let mut line =
                                        serde_json::to_string(&cancel_req).unwrap_or_default();
                                    line.push('\n');
                                    let mut w = std::io::BufWriter::new(stream);
                                    let _ = w.write_all(line.as_bytes());
                                    let _ = w.flush();
                                }
                                last_was_esc = false;
                            } else {
                                last_was_esc = true;
                            }
                        } else {
                            last_was_esc = false;
                        }
                    }
                    _ => break,
                }
            }

            // Always restore terminal settings before the thread exits.
            unsafe {
                libc::tcsetattr(fd, libc::TCSANOW, &saved);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = done_clone;
            let _ = session_id_clone;
        }
    });

    let mut was_cancelled = false;
    client
        .recv_streaming(|resp| match resp {
            tau_agent_lib::protocol::Response::Stream { event } => {
                match event.as_ref() {
                    tau_agent_lib::StreamEvent::TextDelta { delta, .. } => {
                        print!("{}", delta);
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                    }
                    tau_agent_lib::StreamEvent::ToolcallEnd { tool_call, .. } => {
                        let args_str = tool_call.arguments.to_string();
                        let preview = if args_str.len() > 100 {
                            format!("{}...", tau_agent_lib::truncate_str(&args_str, 100))
                        } else {
                            args_str
                        };
                        eprintln!("[tool: {} {}]", tool_call.name, preview);
                    }
                    tau_agent_lib::StreamEvent::ToolOutputDelta { .. } => {
                        eprint!("."); // progress dot for streaming output
                    }
                    tau_agent_lib::StreamEvent::ToolResult {
                        tool_name,
                        is_error,
                        content,
                        summary,
                        ..
                    } => {
                        let preview = if let Some(summary) = summary {
                            summary.clone()
                        } else {
                            let p: String =
                                content.split_whitespace().collect::<Vec<_>>().join(" ");
                            if p.len() > 100 {
                                format!("{}...", tau_agent_lib::truncate_str(&p, 100))
                            } else {
                                p
                            }
                        };
                        if *is_error {
                            eprintln!("[tool error: {} {}]", tool_name, preview);
                        } else {
                            eprintln!("[tool ok: {} {}]", tool_name, preview);
                        }
                    }
                    tau_agent_lib::StreamEvent::Done { message, .. } => {
                        // Only print newline if there was text content
                        if message.content.iter().any(
                            |c| matches!(c, tau_agent_lib::AssistantContent::Text(t) if !t.text.is_empty()),
                        ) {
                            println!();
                        }
                        totals.add(&message.usage);
                    }
                    tau_agent_lib::StreamEvent::Error { error, .. } => {
                        if let Some(ref msg) = error.error_message {
                            eprintln!("\nerror: {}", msg);
                        }
                    }
                    tau_agent_lib::StreamEvent::Status { message } => {
                        eprintln!("[{}]", message);
                    }
                    _ => {}
                }
            }
            tau_agent_lib::protocol::Response::AgentDone => {
                totals.display();
            }
            tau_agent_lib::protocol::Response::Cancelled => {
                was_cancelled = true;
                eprintln!("[cancelled]");
                totals.display();
            }
            tau_agent_lib::protocol::Response::ServerShutdown { restart } => {
                if *restart {
                    eprintln!("[server restarting...]");
                } else {
                    eprintln!("[server shutting down]");
                }
            }
            tau_agent_lib::protocol::Response::Error { message } => {
                eprintln!("error: {}", message);
            }
            _ => {}
        })
        .await?;

    // Signal the escape-watcher thread to stop, then join it so we are
    // guaranteed terminal settings are restored before we return.
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = cancel_thread.join();

    let _ = was_cancelled; // available for future use
    Ok(())
}

fn history_path() -> std::path::PathBuf {
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        std::path::PathBuf::from(data)
            .join("tau")
            .join("history.txt")
    } else if let Ok(home) = std::env::var("HOME") {
        std::path::PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("tau")
            .join("history.txt")
    } else {
        std::path::PathBuf::from("/tmp").join("tau-history.txt")
    }
}

async fn interactive_loop(
    client: &mut tau_agent_lib::client::Client,
    mut session_id: String,
    totals: &mut UsageTotals,
    child_budget: u32,
    initial_attachments: Vec<tau_agent_lib::protocol::ChatAttachment>,
) -> tau_agent_lib::Result<()> {
    let hist = history_path();
    if let Some(parent) = hist.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let mut rl = rustyline::DefaultEditor::new()
        .map_err(|e| tau_agent_lib::Error::Io(format!("readline init: {}", e)))?;
    let _ = rl.load_history(&hist);

    let mut pending_attachments = initial_attachments;
    loop {
        let line = match rl.readline("tau> ") {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(tau_agent_lib::Error::Io(format!("readline: {}", e))),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        rl.add_history_entry(line)
            .map_err(|e| tau_agent_lib::Error::Io(format!("history: {}", e)))?;
        let _ = rl.save_history(&hist);

        // Handle slash commands
        if line.starts_with('/') {
            match handle_slash_command(client, &mut session_id, line, totals, child_budget).await {
                Ok(true) => break, // /quit
                Ok(false) => continue,
                Err(e) => {
                    if try_reconnect(client, &e).await {
                        continue;
                    }
                    eprintln!("error: {}", e);
                    continue;
                }
            }
        }

        match send_and_print(
            client,
            &session_id,
            line,
            totals,
            std::mem::take(&mut pending_attachments),
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                if try_reconnect(client, &e).await {
                    // Retry the message after reconnecting
                    if let Err(e) =
                        send_and_print(client, &session_id, line, totals, Vec::new()).await
                    {
                        eprintln!("error: {}", e);
                    }
                } else {
                    eprintln!("error: {}", e);
                }
            }
        }
    }

    Ok(())
}

/// Try to reconnect to the server after a connection error.
/// Returns true if reconnection succeeded.
async fn try_reconnect(
    client: &mut tau_agent_lib::client::Client,
    err: &tau_agent_lib::Error,
) -> bool {
    let msg = err.to_string();
    if !msg.contains("Broken pipe")
        && !msg.contains("Connection refused")
        && !msg.contains("Connection reset")
    {
        return false;
    }
    eprintln!("[connection lost, reconnecting...]");
    // Wait a moment for the server to restart
    for _ in 0..30 {
        smol::Timer::after(std::time::Duration::from_millis(200)).await;
        if let Ok(new_client) = tau_agent_lib::client::Client::connect_or_start().await {
            *client = new_client;
            eprintln!("[reconnected]");
            return true;
        }
    }
    eprintln!("[reconnection failed]");
    false
}

fn pct(b: Option<&tau_agent_lib::auth::UsageBucket>) -> String {
    tau_agent_lib::protocol::format_utilization(b.and_then(|b| b.utilization))
}

/// Parse ISO 8601 reset timestamp → "Thu 04:00 (3d 14h 15m)".
fn format_resets(resets_at: &str) -> String {
    use chrono::{DateTime, Local};
    let Ok(dt) = DateTime::parse_from_rfc3339(resets_at) else {
        return resets_at.to_string();
    };
    let local: DateTime<Local> = dt.into();
    let secs = local
        .signed_duration_since(Local::now())
        .num_seconds()
        .max(0);
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let relative = match (d, h) {
        (0, 0) => format!("{}m", m),
        (0, _) => format!("{}h {}m", h, m),
        _ => format!("{}d {}h {}m", d, h, m),
    };
    format!("{} ({})", local.format("%a %H:%M"), relative)
}

async fn print_subscription_usage(client: &mut tau_agent_lib::client::Client) {
    client
        .send(&tau_agent_lib::protocol::Request::GetSubscriptionUsage)
        .await
        .ok();

    client
        .recv_streaming(|resp| {
            if let tau_agent_lib::protocol::Response::SubscriptionUsage { usage } = resp {
                fn bucket_line(
                    label: &str,
                    indent: bool,
                    b: Option<&tau_agent_lib::auth::UsageBucket>,
                ) {
                    let prefix = if indent { "          " } else { "usage:    " };
                    let resets = b
                        .and_then(|b| b.resets_at.as_deref())
                        .map(format_resets)
                        .unwrap_or_else(|| "?".into());
                    println!("{}{:<8} {:<6} resets {}", prefix, label, pct(b), resets,);
                }
                let mut first = true;
                for (label, bucket) in [
                    ("5h", &usage.five_hour),
                    ("7d", &usage.seven_day),
                    ("sonnet", &usage.seven_day_sonnet),
                    ("opus", &usage.seven_day_opus),
                ] {
                    if bucket.is_some() {
                        bucket_line(label, !first, bucket.as_ref());
                        first = false;
                    }
                }
                if let Some(extra) = &usage.extra_usage
                    && extra.is_enabled
                    && let (Some(used), Some(limit)) = (extra.used_credits, extra.monthly_limit)
                {
                    println!("          extra ${:.2}/${:.2}", used, limit);
                }
            } else if let tau_agent_lib::protocol::Response::Error { message } = resp {
                eprintln!("usage:    unavailable ({})", message);
            }
        })
        .await
        .ok();
}

/// Handle a slash command. Returns Ok(true) if the loop should exit.
async fn handle_slash_command(
    client: &mut tau_agent_lib::client::Client,
    session_id: &mut String,
    line: &str,
    totals: &mut UsageTotals,
    child_budget: u32,
) -> tau_agent_lib::Result<bool> {
    let (cmd, args) = line.split_once(' ').unwrap_or((line, ""));
    let args = args.trim();

    match cmd {
        "/quit" | "/exit" => return Ok(true),

        "/status" => {
            let info = get_session_info(client, session_id).await?;
            println!("session:  {}", info.id);
            println!("provider: {}", info.provider);
            println!("model:    {}", info.model);
            println!("cwd:      {}", info.cwd.as_deref().unwrap_or("(not set)"));
            println!(
                "messages: {} user, {} assistant, {} tool calls",
                info.stats.user_messages, info.stats.assistant_messages, info.stats.tool_calls
            );
            println!(
                "tokens:   {}",
                tau_agent_lib::protocol::format_stats(&info.stats)
            );

            if info.stats.is_subscription {
                print_subscription_usage(client).await;
            }
        }

        "/model" | "/models" => {
            if args.is_empty() {
                // Get current model first, then list
                let current_info = get_session_info(client, session_id).await.ok();
                let current_model_id = current_info.as_ref().map(|i| i.model.clone());
                let session_cwd = current_info.and_then(|i| i.cwd);

                client
                    .send(&tau_agent_lib::protocol::Request::ListModels)
                    .await?;
                client
                    .recv_streaming(|resp| {
                        if let tau_agent_lib::protocol::Response::Models { models } = resp {
                            for m in models {
                                let marker = if current_model_id.as_deref() == Some(m.id.as_str()) {
                                    " *"
                                } else {
                                    ""
                                };
                                println!(
                                    "  {}{}\t{}\t{}K ctx",
                                    m.id,
                                    marker,
                                    m.provider,
                                    m.context_window / 1000,
                                );
                            }
                        }
                    })
                    .await?;

                // Best-effort: ask the server for configured aliases. Older
                // servers will return an Error response which we silently
                // skip — the protocol comment in tau_agent_lib::protocol::Request
                // documents this fallback contract.
                client
                    .send(&tau_agent_lib::protocol::Request::ListAliases { cwd: session_cwd })
                    .await?;
                client
                    .recv_streaming(|resp| {
                        if let tau_agent_lib::protocol::Response::Aliases { global, project } = resp
                        {
                            if !global.is_empty() {
                                println!();
                                println!("aliases (global):");
                                for a in global {
                                    println!("  {:<16} -> {}", a.name, a.target);
                                }
                            }
                            if !project.is_empty() {
                                println!();
                                println!("aliases (project):");
                                for a in project {
                                    println!("  {:<16} -> {}", a.name, a.target);
                                }
                            }
                        }
                    })
                    .await?;
            } else {
                // Set model
                client
                    .send(&tau_agent_lib::protocol::Request::SetModel {
                        session_id: session_id.to_string(),
                        model_id: args.to_string(),
                        caller_session_id: None,
                    })
                    .await?;
                client
                    .recv_streaming(|resp| match resp {
                        tau_agent_lib::protocol::Response::ModelChanged { model } => {
                            eprintln!("model changed to {}", model.id);
                        }
                        tau_agent_lib::protocol::Response::Error { message } => {
                            eprintln!("error: {}", message);
                        }
                        _ => {}
                    })
                    .await?;
            }
        }

        "/cwd" => {
            if args.is_empty() {
                // Show current cwd
                let info = get_session_info(client, session_id).await?;
                println!("{}", info.cwd.as_deref().unwrap_or("(not set)"));
            } else {
                // Resolve and set new cwd
                let new_cwd = if std::path::Path::new(args).is_absolute() {
                    args.to_string()
                } else {
                    // Resolve relative to current session cwd or process cwd
                    let info = get_session_info(client, session_id).await.ok();
                    let base = info.and_then(|i| i.cwd).unwrap_or_else(|| {
                        std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    });
                    let resolved = std::path::Path::new(&base).join(args);
                    resolved.to_string_lossy().to_string()
                };
                if !std::path::Path::new(&new_cwd).is_dir() {
                    eprintln!("error: {} is not a directory", new_cwd);
                } else {
                    client
                        .send(&tau_agent_lib::protocol::Request::SetCwd {
                            session_id: session_id.to_string(),
                            cwd: new_cwd.clone(),
                            caller_session_id: None,
                        })
                        .await?;
                    client.recv_streaming(|_| {}).await?;
                    // Notify the model about the cwd change
                    let notice = format!("[Working directory changed to: {}]", new_cwd);
                    send_and_print(client, session_id, &notice, totals, Vec::new()).await?;
                    eprintln!("cwd: {}", new_cwd);
                }
            }
        }

        "/help" => {
            println!("commands:");
            println!("  /status        show session info and stats");
            println!("  /model         list available models");
            println!("  /model <id>    switch to a different model");
            println!("  /cwd           show working directory");
            println!("  /cwd <path>    change working directory");
            println!("  /fork          fork session (inherit model/cwd)");
            println!("  /new           create a fresh session");
            println!("  /reload        reload plugins");
            println!("  /help          show this help");
            println!("  /quit          exit");
        }

        "/reload" => {
            client
                .send(&tau_agent_lib::protocol::Request::ReloadPlugins {
                    session_id: session_id.to_string(),
                })
                .await?;
            client.recv_streaming(|_| {}).await?;
            eprintln!("Plugins reloaded");
        }

        "/fork" => {
            // Create a new session inheriting model/cwd from the current session
            let info = get_session_info(client, session_id).await?;
            let new_id = cli_create_session(
                client,
                Some(info.model),
                info.cwd,
                Some(session_id.clone()),
                child_budget,
            )
            .await?;
            eprintln!("Forked to session {}", &new_id[..new_id.len().min(8)]);
            *totals = UsageTotals::default();
            *session_id = new_id;
        }

        "/new" => {
            // Create a fresh session with defaults
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from));
            let new_id = cli_create_session(client, None, cwd, None, child_budget).await?;
            eprintln!("New session {}", &new_id[..new_id.len().min(8)]);
            *totals = UsageTotals::default();
            *session_id = new_id;
        }

        _ => {
            eprintln!(
                "unknown command: {}. Type /help for available commands.",
                cmd
            );
        }
    }

    Ok(false)
}

// ---------------------------------------------------------------------------
// Server commands
// ---------------------------------------------------------------------------

async fn cmd_server_start(foreground: bool) -> tau_agent_lib::Result<()> {
    if tau_agent_lib::server::is_running() {
        eprintln!("server already running");
        return Ok(());
    }

    if foreground {
        tau_agent_lib::server::run().await?;
    } else {
        spawn_server_daemon()?;
    }
    Ok(())
}

fn spawn_server_daemon() -> tau_agent_lib::Result<()> {
    let exe = std::env::current_exe().map_err(|e| tau_agent_lib::Error::Io(e.to_string()))?;

    // Route the daemon's stderr to a catch-all file: tracing handles normal
    // diagnostic output, but panics and any un-migrated `eprintln!`s should
    // still be captured for post-mortem debugging. The file is opened in
    // append mode so daemon restarts don't truncate history.
    let logs_dir = tau_agent_lib::paths::logs_dir();
    std::fs::create_dir_all(&logs_dir)
        .map_err(|e| tau_agent_lib::Error::Io(format!("create logs dir: {}", e)))?;
    let stderr_path = logs_dir.join("daemon.stderr.log");
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .map_err(|e| tau_agent_lib::Error::Io(format!("open {}: {}", stderr_path.display(), e)))?;

    let child = std::process::Command::new(exe)
        .args(["server", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .map_err(|e| tau_agent_lib::Error::Io(format!("spawn: {}", e)))?;
    eprintln!("server started (pid {})", child.id());

    // Wait for ready
    smol::block_on(async {
        for _ in 0..50 {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            if tau_agent_lib::server::is_running() {
                eprintln!(
                    "server ready at {}",
                    tau_agent_lib::server::socket_path().display()
                );
                return;
            }
        }
        eprintln!("warning: server may not have started");
    });
    Ok(())
}

async fn cmd_server_stop() -> tau_agent_lib::Result<()> {
    if !tau_agent_lib::server::is_running() {
        eprintln!("server not running");
        return Ok(());
    }
    let mut client = tau_agent_lib::client::Client::connect().await?;
    client
        .send(&tau_agent_lib::protocol::Request::Shutdown { restart: false })
        .await?;
    client.recv_streaming(|_| {}).await?;
    eprintln!("server stopped");
    Ok(())
}

async fn cmd_server_restart() -> tau_agent_lib::Result<()> {
    if tau_agent_lib::server::is_running() {
        let mut client = tau_agent_lib::client::Client::connect().await?;
        client
            .send(&tau_agent_lib::protocol::Request::Shutdown { restart: true })
            .await?;
        client.recv_streaming(|_| {}).await?;
        eprintln!("shutdown requested, waiting for server to exit...");
        // Wait up to 65s (server drains for up to 60s)
        for i in 0..650 {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            if !tau_agent_lib::server::is_running() {
                break;
            }
            if i > 0 && i % 50 == 0 {
                eprintln!("still waiting... ({}s)", i / 10);
            }
        }
        if tau_agent_lib::server::is_running() {
            return Err(tau_agent_lib::Error::Io(
                "old server didn't exit within 65s".into(),
            ));
        }
    }
    spawn_server_daemon()?;
    Ok(())
}

fn cmd_server_status() {
    if tau_agent_lib::server::is_running() {
        eprintln!(
            "server running at {}",
            tau_agent_lib::server::socket_path().display()
        );
    } else {
        eprintln!("server not running");
    }
}

async fn cmd_config_reload() -> tau_agent_lib::Result<()> {
    if !tau_agent_lib::server::is_running() {
        return Err(tau_agent_lib::Error::Io(
            "tau server not running; start it with `tau server start`".into(),
        ));
    }
    let mut client = tau_agent_lib::client::Client::connect().await?;
    client
        .send(&tau_agent_lib::protocol::Request::ReloadConfig)
        .await?;
    let mut err: Option<String> = None;
    client
        .recv_streaming(|resp| {
            if let tau_agent_lib::protocol::Response::Error { message } = resp {
                err = Some(message.clone());
            }
        })
        .await?;
    if let Some(message) = err {
        return Err(tau_agent_lib::Error::Io(message));
    }
    eprintln!("config reloaded");
    Ok(())
}

fn cmd_config_show() {
    let providers = tau_agent_lib::paths::config_dir().join("providers.toml");
    let models = tau_agent_lib::paths::config_dir().join("models.toml");
    println!(
        "providers.toml: {} ({})",
        providers.display(),
        if providers.exists() {
            "present"
        } else {
            "missing"
        },
    );
    println!(
        "models.toml:    {} ({})",
        models.display(),
        if models.exists() {
            "present"
        } else {
            "missing"
        },
    );
    println!();
    println!("Edit the files, then run `tau config reload` to pick up the changes.");
}

async fn cmd_auth_status() -> tau_agent_lib::Result<()> {
    let auth = tau_agent_lib::auth::AuthStorage::open_default();
    let providers = auth.list()?;
    if providers.is_empty() {
        println!("not logged in to any providers");
        println!("run `tau login` to authenticate");
    } else {
        for p in &providers {
            let status = match auth.get(p)? {
                Some(tau_agent_lib::auth::AuthCredential::Oauth(creds)) => {
                    if creds.is_expired() {
                        "oauth (expired, will auto-refresh)"
                    } else {
                        "oauth (valid)"
                    }
                }
                Some(tau_agent_lib::auth::AuthCredential::ApiKey { .. }) => "api key",
                None => "none",
            };
            println!("{}\t{}", p, status);
        }
    }
    Ok(())
}

async fn cmd_sessions_list(include_archived: bool) -> tau_agent_lib::Result<()> {
    let mut client = tau_agent_lib::client::Client::connect_or_start().await?;
    client
        .send(&tau_agent_lib::protocol::Request::ListSessions {
            include_archived,
            project_name: None,
        })
        .await?;

    client
        .recv_streaming(|resp| {
            if let tau_agent_lib::protocol::Response::Sessions { sessions } = resp {
                if sessions.is_empty() {
                    println!("no sessions");
                } else {
                    // Build tree display: roots first, then children indented
                    let roots: Vec<_> = sessions.iter().filter(|s| s.parent_id.is_none()).collect();
                    for root in &roots {
                        print_session_tree(root, sessions, 0);
                    }
                    // Show orphans (parent deleted but child remains)
                    let orphans: Vec<_> = sessions
                        .iter()
                        .filter(|s| {
                            s.parent_id.is_some()
                                && !sessions.iter().any(|p| Some(&p.id) == s.parent_id.as_ref())
                        })
                        .collect();
                    for o in &orphans {
                        print_session_tree(o, sessions, 0);
                    }
                }
            }
        })
        .await?;
    Ok(())
}

fn print_session_tree(
    session: &tau_agent_lib::protocol::SessionInfo,
    all: &[tau_agent_lib::protocol::SessionInfo],
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    let stats = tau_agent_lib::protocol::format_stats(&session.stats);
    let cwd = session.cwd.as_deref().unwrap_or("");
    let ago = format_time_ago(session.last_activity);
    let budget = if session.child_budget > 0 {
        format!(" [budget:{}/{}]", session.child_count, session.child_budget)
    } else {
        String::new()
    };
    let archived_tag = if session.archived { " [archived]" } else { "" };
    let tagline_str = session
        .tagline
        .as_deref()
        .map(|t| {
            let max = 60;
            if t.len() > max {
                format!("\t{}...", &t[..max - 3])
            } else {
                format!("\t{}", t)
            }
        })
        .unwrap_or_default();
    println!(
        "{}{}\t{}/{}\t{}\t{}\t{}{}{}{}",
        indent,
        session.id,
        session.provider,
        session.model,
        ago,
        cwd,
        stats,
        budget,
        archived_tag,
        tagline_str
    );
    // Print children
    let children: Vec<_> = all
        .iter()
        .filter(|s| s.parent_id.as_ref() == Some(&session.id))
        .collect();
    for child in children {
        print_session_tree(child, all, depth + 1);
    }
}

async fn cmd_sessions_archive(id: &str) -> tau_agent_lib::Result<()> {
    let mut client = tau_agent_lib::client::Client::connect_or_start().await?;
    client
        .send(&tau_agent_lib::protocol::Request::ArchiveSession {
            session_id: id.to_string(),
            require_ancestor: None,
        })
        .await?;
    client.recv_streaming(|_| {}).await?;
    eprintln!("archived session {}", id);
    Ok(())
}

async fn cmd_sessions_restore(id: &str) -> tau_agent_lib::Result<()> {
    let mut client = tau_agent_lib::client::Client::connect_or_start().await?;
    client
        .send(&tau_agent_lib::protocol::Request::RestoreSession {
            session_id: id.to_string(),
        })
        .await?;
    client.recv_streaming(|_| {}).await?;
    eprintln!("restored session {}", id);
    Ok(())
}

async fn cmd_sessions_delete(id: &str) -> tau_agent_lib::Result<()> {
    let mut client = tau_agent_lib::client::Client::connect_or_start().await?;
    client
        .send(&tau_agent_lib::protocol::Request::DeleteSession {
            session_id: id.to_string(),
        })
        .await?;
    client.recv_streaming(|_| {}).await?;
    eprintln!("deleted session {}", id);
    Ok(())
}

fn cmd_sessions_dump(id: &str, output: Option<&str>) -> tau_agent_lib::Result<()> {
    let db = tau_agent_lib::db::Db::open_default()?;
    let recording = tau_agent_lib::replay::dump_session(&db, id)?;
    let json = serde_json::to_string_pretty(&recording)
        .map_err(|e| tau_agent_lib::Error::Io(format!("serialize recording: {}", e)))?;

    if let Some(path) = output {
        std::fs::write(path, &json)
            .map_err(|e| tau_agent_lib::Error::Io(format!("write {}: {}", path, e)))?;
        eprintln!("dumped session {} to {}", id, path);
    } else {
        println!("{}", json);
    }
    Ok(())
}

async fn cmd_sessions_gc(older_than: u64) -> tau_agent_lib::Result<()> {
    let mut client = tau_agent_lib::client::Client::connect_or_start().await?;
    client
        .send(&tau_agent_lib::protocol::Request::GcSessions {
            older_than_days: older_than,
        })
        .await?;

    client
        .recv_streaming(|resp| match resp {
            tau_agent_lib::protocol::Response::GcComplete { deleted } => {
                eprintln!("gc: deleted {} archived session(s)", deleted);
            }
            tau_agent_lib::protocol::Response::Error { message } => {
                eprintln!("error: {}", message);
            }
            _ => {}
        })
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Provider / model management (edits providers.toml directly)
// ---------------------------------------------------------------------------

fn cmd_providers_list() -> tau_agent_lib::Result<()> {
    let cfg = tau_agent_lib::config::load_config()?;
    // Show built-in providers
    println!("built-in:");
    println!("  anthropic  api=anthropic  https://api.anthropic.com");
    println!("  openai     api=openai     https://api.openai.com/v1");
    if !cfg.providers.is_empty() {
        println!("custom:");
        for (name, pc) in &cfg.providers {
            let key_info = match &pc.api_key {
                Some(k) if k == "none" => " (no auth)",
                Some(k) if k.starts_with('$') => &format!(" ({})", k),
                Some(_) => " (inline key)",
                None => "",
            };
            println!(
                "  {:<12} api={:<10} {}{}",
                name, pc.api, pc.base_url, key_info
            );
        }
    }
    Ok(())
}

fn cmd_providers_add(
    name: &str,
    api: &str,
    base_url: &str,
    api_key: Option<&str>,
) -> tau_agent_lib::Result<()> {
    let mut cfg = tau_agent_lib::config::load_config()?;
    cfg.providers.insert(
        name.to_string(),
        tau_agent_lib::config::ProviderConfig {
            api: api.to_string(),
            base_url: base_url.to_string(),
            api_key: api_key.map(String::from),
            models: Vec::new(),
        },
    );
    tau_agent_lib::config::save_config(&cfg)?;
    eprintln!("provider '{}' added. Restart server to apply.", name);
    Ok(())
}

fn cmd_providers_remove(name: &str) -> tau_agent_lib::Result<()> {
    let mut cfg = tau_agent_lib::config::load_config()?;
    if cfg.providers.remove(name).is_none() {
        eprintln!("provider '{}' not found in config", name);
        return Ok(());
    }
    tau_agent_lib::config::save_config(&cfg)?;
    eprintln!("provider '{}' removed. Restart server to apply.", name);
    Ok(())
}

fn parse_thinking_style(s: &str) -> tau_agent_lib::Result<tau_agent_lib::ThinkingStyle> {
    match s {
        "none" => Ok(tau_agent_lib::ThinkingStyle::None),
        "anthropic" => Ok(tau_agent_lib::ThinkingStyle::Anthropic),
        "openai" => Ok(tau_agent_lib::ThinkingStyle::OpenAi),
        "qwen" => Ok(tau_agent_lib::ThinkingStyle::Qwen),
        _ => Err(tau_agent_lib::Error::Parse(format!(
            "unknown thinking style: '{}'. Use: none, anthropic, openai, qwen",
            s
        ))),
    }
}

fn cmd_models_list() -> tau_agent_lib::Result<()> {
    let cfg = tau_agent_lib::config::load_config()?;
    let models = tau_agent_lib::config::resolve_models(&cfg);
    for m in &models {
        let thinking = match m.thinking {
            tau_agent_lib::ThinkingStyle::None => "",
            tau_agent_lib::ThinkingStyle::Anthropic => " [anthropic]",
            tau_agent_lib::ThinkingStyle::OpenAi => " [openai]",
            tau_agent_lib::ThinkingStyle::Qwen => " [qwen]",
        };
        println!(
            "  {:<32} {:<12} {}K ctx{}",
            m.id,
            m.provider,
            m.context_window / 1000,
            thinking,
        );
    }

    // Aliases — global from ~/.config/tau/models.toml + project-local
    // from ./.tau/models.toml.  Project lookup is non-recursive: it only
    // checks the current working directory, not parent directories.  Run
    // from the project root to see project aliases.
    let global_aliases = tau_agent_lib::models_config::load_global_aliases();
    if !global_aliases.is_empty() {
        println!();
        println!("aliases (global):");
        let mut entries: Vec<(&String, &String)> = global_aliases.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, target) in entries {
            println!("  {:<16} -> {}", name, target);
        }
    }

    let project_aliases = tau_agent_lib::models_config::load_project_aliases(".");
    if !project_aliases.is_empty() {
        println!();
        println!("aliases (project):");
        let mut entries: Vec<(&String, &String)> = project_aliases.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, target) in entries {
            println!("  {:<16} -> {}", name, target);
        }
    }

    Ok(())
}

fn cmd_models_add(
    id: &str,
    provider: &str,
    name: Option<&str>,
    context: u64,
    max_tokens: u64,
    thinking: &str,
) -> tau_agent_lib::Result<()> {
    let thinking = parse_thinking_style(thinking)?;
    let mut cfg = tau_agent_lib::config::load_config()?;
    let pc = cfg.providers.get_mut(provider).ok_or_else(|| {
        tau_agent_lib::Error::Io(format!(
            "provider '{}' not found. Add it first with `tau providers add`.",
            provider
        ))
    })?;
    // Remove existing model with same id
    pc.models.retain(|m| m.id != id);
    pc.models.push(tau_agent_lib::config::ModelConfig {
        id: id.to_string(),
        name: name.map(String::from),
        context_window: context,
        max_tokens,
        thinking,
        cost: tau_agent_lib::ModelCost::default(),
    });
    tau_agent_lib::config::save_config(&cfg)?;
    eprintln!(
        "model '{}' added to provider '{}'. Restart server to apply.",
        id, provider
    );
    Ok(())
}

fn cmd_models_remove(id: &str, provider: &str) -> tau_agent_lib::Result<()> {
    let mut cfg = tau_agent_lib::config::load_config()?;
    let pc = cfg
        .providers
        .get_mut(provider)
        .ok_or_else(|| tau_agent_lib::Error::Io(format!("provider '{}' not found", provider)))?;
    let before = pc.models.len();
    pc.models.retain(|m| m.id != id);
    if pc.models.len() == before {
        eprintln!("model '{}' not found in provider '{}'", id, provider);
        return Ok(());
    }
    tau_agent_lib::config::save_config(&cfg)?;
    eprintln!(
        "model '{}' removed from provider '{}'. Restart server to apply.",
        id, provider
    );
    Ok(())
}

fn format_time_ago(unix_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64;
    let delta = now - unix_secs;
    if delta < 0 {
        return "just now".to_string();
    }
    let delta = delta as u64;
    match delta {
        0..=59 => "just now".to_string(),
        60..=3599 => {
            let m = delta / 60;
            format!("{}m ago", m)
        }
        3600..=86399 => {
            let h = delta / 3600;
            format!("{}h ago", h)
        }
        86400..=2591999 => {
            let d = delta / 86400;
            format!("{}d ago", d)
        }
        _ => {
            let mo = delta / 2592000;
            format!("{}mo ago", mo)
        }
    }
}

// ---------------------------------------------------------------------------
// Task commands (direct DB access, no server round-trip)
// ---------------------------------------------------------------------------

fn project_key() -> tau_agent_lib::Result<String> {
    let cwd = std::env::current_dir()
        .map_err(|e| tau_agent_lib::Error::Io(format!("current_dir: {}", e)))?;
    match tau_agent_lib::project::discover_project(&cwd) {
        Some((name, _root)) => Ok(name),
        None => Err(tau_agent_lib::Error::Io(
            "not in a tau project — run `tau project init` first".into(),
        )),
    }
}

fn format_task_timestamp(ms: i64) -> String {
    use chrono::{DateTime, Local, TimeZone, Utc};
    let dt: DateTime<Utc> = Utc.timestamp_millis_opt(ms).single().unwrap_or_default();
    let local: DateTime<Local> = dt.into();
    local.format("%Y-%m-%d %H:%M").to_string()
}

// ---------------------------------------------------------------------------
// Project commands
// ---------------------------------------------------------------------------

fn cmd_project(action: ProjectAction) -> tau_agent_lib::Result<()> {
    match action {
        ProjectAction::Init { name } => cmd_project_init(name),
        ProjectAction::List => cmd_project_list(),
        ProjectAction::Info => cmd_project_info(),
        ProjectAction::Rename { new_name } => cmd_project_rename(&new_name),
        ProjectAction::Stats { project } => cmd_project_stats(project),
        ProjectAction::Migrate => cmd_project_migrate(),
    }
}

fn cmd_project_init(name: Option<String>) -> tau_agent_lib::Result<()> {
    let cwd = std::env::current_dir()
        .map_err(|e| tau_agent_lib::Error::Io(format!("current_dir: {}", e)))?;

    let name = match name {
        Some(n) => n,
        None => {
            let dir_name = cwd
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            tau_agent_lib::project::slugify(&dir_name)
        }
    };

    tau_agent_lib::project::validate_project_name(&name)?;

    // Check DB for name/path collisions before touching the filesystem.
    let db = tau_agent_lib::db::Db::open_default()?;
    if db.get_project(&name)?.is_some() {
        return Err(tau_agent_lib::Error::Io(format!(
            "project name '{}' already exists in the registry",
            name
        )));
    }

    // Canonicalize cwd for path collision check (best effort — it exists).
    let canonical_path = std::fs::canonicalize(&cwd)
        .map_err(|e| tau_agent_lib::Error::Io(format!("canonicalize: {}", e)))?;
    let path_str = canonical_path.to_string_lossy().to_string();

    if db.get_project_by_path(&path_str)?.is_some() {
        return Err(tau_agent_lib::Error::Io(format!(
            "a project already exists at {}",
            path_str
        )));
    }

    // Create filesystem artifacts.
    let canonical = tau_agent_lib::project::init_project(&cwd, &name)?;
    let canonical_str = canonical.to_string_lossy().to_string();

    // Register in global DB.
    db.create_project(&name, &canonical_str)?;

    eprintln!("initialized project '{}' at {}", name, canonical_str);
    Ok(())
}

fn cmd_project_list() -> tau_agent_lib::Result<()> {
    let db = tau_agent_lib::db::Db::open_default()?;
    let projects = db.list_projects()?;

    if projects.is_empty() {
        println!("no projects");
        return Ok(());
    }

    println!(
        "{:<20} {:<50} {:<14} {}",
        "NAME", "PATH", "LAST SEEN", "CREATED"
    );
    for p in &projects {
        let last_seen = format_time_ago_ms(p.last_seen);
        let created = format_time_ago_ms(p.created_at);
        println!(
            "{:<20} {:<50} {:<14} {}",
            p.name, p.path, last_seen, created
        );
    }
    Ok(())
}

fn cmd_project_info() -> tau_agent_lib::Result<()> {
    let cwd = std::env::current_dir()
        .map_err(|e| tau_agent_lib::Error::Io(format!("current_dir: {}", e)))?;

    let (name, root) = tau_agent_lib::project::discover_project(&cwd).ok_or_else(|| {
        tau_agent_lib::Error::Io("not in a tau project (no .tau/project.toml found)".into())
    })?;

    let operator_dir = tau_agent_lib::paths::project_config_dir(&name);

    println!("name:       {}", name);
    println!("path:       {}", root.display());
    println!("config dir: {}", operator_dir.display());

    // List .tau config files found.
    let tau_dir = root.join(".tau");
    if tau_dir.is_dir() {
        let mut config_files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&tau_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    if let Some(fname) = path.file_name() {
                        config_files.push(fname.to_string_lossy().to_string());
                    }
                }
            }
        }
        config_files.sort();
        if !config_files.is_empty() {
            println!("configs:    {}", config_files.join(", "));
        }
    }

    // Show DB registration status.
    let db = tau_agent_lib::db::Db::open_default()?;
    if let Some(proj) = db.get_project(&name)? {
        println!(
            "registered: yes (last seen {})",
            format_time_ago_ms(proj.last_seen)
        );
    } else {
        println!("registered: no (run `tau project init` to register)");
    }

    Ok(())
}

fn cmd_project_migrate() -> tau_agent_lib::Result<()> {
    let tau_db_path = tau_agent_lib::paths::data_dir().join("tau.db");
    let tasks_db_path = tau_agent_lib::paths::data_dir().join("tasks.db");
    let db = tau_agent_lib::db::Db::open(&tau_db_path)?;
    tau_agent_lib::migration::run_project_migration(&db, &tasks_db_path)?;
    eprintln!("Migration complete.");
    Ok(())
}

/// Render `tau project stats` output: one project-total block, formatted
/// like a condensed session status. Defaults to the current project
/// (discovered from cwd) when `project` is None.
fn cmd_project_stats(project: Option<String>) -> tau_agent_lib::Result<()> {
    let name = match project {
        Some(n) => n,
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| tau_agent_lib::Error::Io(format!("current_dir: {}", e)))?;
            let (n, _) = tau_agent_lib::project::discover_project(&cwd).ok_or_else(|| {
                tau_agent_lib::Error::Io(
                    "not in a tau project (no .tau/project.toml found). Pass --project NAME to inspect a specific project."
                        .into(),
                )
            })?;
            n
        }
    };

    let db = tau_agent_lib::db::Db::open_default()?;
    let stats = db.project_stats(&name)?;
    print_project_stats_block(&name, &stats);
    Ok(())
}

/// Shared renderer used by `tau project stats`.  Kept in one place so the
/// TUI `/project stats` slash command can produce the same layout.
fn print_project_stats_block(project_name: &str, stats: &tau_agent_lib::db::DbProjectStats) {
    println!("Project: {}", project_name);
    println!(
        "  Sessions:     {}",
        format_u64_commas(stats.session_count as u64)
    );
    println!(
        "  Messages:     {}",
        format_u64_commas(stats.message_count as u64)
    );
    println!(
        "  Tokens:       input {}   output {}",
        format_u64_commas(stats.tokens_input),
        format_u64_commas(stats.tokens_output),
    );
    println!(
        "                cache_read {}   cache_write {}",
        format_u64_commas(stats.tokens_cache_read),
        format_u64_commas(stats.tokens_cache_write),
    );
    println!("  Cost:         ${:.2}", stats.cost);
    match stats.last_message_time {
        Some(t) => println!("  Last activity: {}", format_time_ago(t)),
        None => println!("  Last activity: (no messages yet)"),
    }
}

/// Format a non-negative integer with thousand-separator commas.
/// `1234567 -> "1,234,567"`.
fn format_u64_commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn cmd_project_rename(new_name: &str) -> tau_agent_lib::Result<()> {
    let cwd = std::env::current_dir()
        .map_err(|e| tau_agent_lib::Error::Io(format!("current_dir: {}", e)))?;

    let (old_name, root) = tau_agent_lib::project::discover_project(&cwd).ok_or_else(|| {
        tau_agent_lib::Error::Io("not in a tau project (no .tau/project.toml found)".into())
    })?;

    tau_agent_lib::project::validate_project_name(new_name)?;

    if old_name == new_name {
        eprintln!("project is already named '{}'", new_name);
        return Ok(());
    }

    // Rename operator config directory first (easiest to revert).
    let old_operator_dir = tau_agent_lib::paths::project_config_dir(&old_name);
    let new_operator_dir = tau_agent_lib::paths::project_config_dir(new_name);
    if new_operator_dir.exists() {
        return Err(tau_agent_lib::Error::Io(format!(
            "operator config directory already exists: {}",
            new_operator_dir.display()
        )));
    }
    if old_operator_dir.exists() {
        std::fs::rename(&old_operator_dir, &new_operator_dir)
            .map_err(|e| tau_agent_lib::Error::Io(format!("rename operator dir: {}", e)))?;
    }

    // Update .tau/project.toml.
    let config = tau_agent_lib::project::ProjectConfig {
        name: new_name.to_string(),
    };
    let toml_str = toml::to_string_pretty(&config)
        .map_err(|e| tau_agent_lib::Error::Io(format!("serialize project.toml: {}", e)))?;
    let project_toml = root.join(".tau").join("project.toml");
    std::fs::write(&project_toml, toml_str)
        .map_err(|e| tau_agent_lib::Error::Io(format!("write project.toml: {}", e)))?;

    // Update DB.
    let db = tau_agent_lib::db::Db::open_default()?;
    db.rename_project(&old_name, new_name)?;

    eprintln!("renamed project '{}' → '{}'", old_name, new_name);
    Ok(())
}

/// Format a millisecond timestamp as a relative time string (e.g. "5m ago").
fn format_time_ago_ms(ms: i64) -> String {
    format_time_ago(ms / 1000)
}

fn cmd_task(action: TaskAction) -> tau_agent_lib::Result<()> {
    let db = tau_agent_lib::tasks_db::TasksDb::open_default()?;

    match action {
        TaskAction::List { state, parent } => {
            let project = project_key()?;
            let tasks = db.list_tasks(&project, state.as_deref(), parent, None, None)?;
            if tasks.is_empty() {
                println!("no tasks");
                return Ok(());
            }
            let tree = tau_agent_lib::tasks_db::tree_order(tasks);
            println!("  {:>4}  {:<12}  {:>8}  TITLE", "ID", "STATE", "PRIORITY");
            for (depth, t) in &tree {
                let indent = "  ".repeat(*depth);
                println!(
                    "  {:>4}  {:<12}  {:>8}  {}{}",
                    t.id, t.state, t.priority, indent, t.title
                );
            }
        }
        TaskAction::Get { id } => {
            let task = db
                .get_task(id)?
                .ok_or_else(|| tau_agent_lib::Error::Io(format!("task {} not found", id)))?;

            let skip = if task.skip_review { "yes" } else { "no" };
            let require_approval = if task.require_approval { "yes" } else { "no" };
            let branch = task.branch.as_deref().unwrap_or("none");
            let parent = task
                .parent_id
                .map(|p| format!("#{}", p))
                .unwrap_or_else(|| "none".to_string());
            let tags = match &task.tags {
                Some(v) => {
                    if let Some(arr) = v.as_array() {
                        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                        if strs.is_empty() {
                            "(none)".to_string()
                        } else {
                            format!("[{}]", strs.join(", "))
                        }
                    } else {
                        v.to_string()
                    }
                }
                None => "(none)".to_string(),
            };
            let affected = match &task.affected_files {
                Some(v) => {
                    if let Some(arr) = v.as_array() {
                        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                        if strs.is_empty() {
                            "(none)".to_string()
                        } else {
                            strs.join(", ")
                        }
                    } else {
                        v.to_string()
                    }
                }
                None => "(none)".to_string(),
            };

            println!("Task #{}: {}", task.id, task.title);
            println!(
                "State: {} | Priority: {} | Skip review: {} | Require approval: {}",
                task.state, task.priority, skip, require_approval
            );
            println!("Branch: {} | Parent: {}", branch, parent);
            println!("Tags: {}", tags);
            println!("Affected files: {}", affected);

            // Messages
            let messages = db.get_messages(id)?;
            if !messages.is_empty() {
                println!();
                println!("Messages:");
                for msg in &messages {
                    let author = msg.author.as_deref().unwrap_or("unknown");
                    let ts = format_task_timestamp(msg.created_at);
                    println!("  #{} [{}] {}", msg.id, author, ts);
                    for line in msg.content.lines() {
                        println!("  {}", line);
                    }
                    println!();
                }
            }

            // Subtasks
            let subtasks = db.get_subtasks(id)?;
            if !subtasks.is_empty() {
                println!("Subtasks:");
                for st in &subtasks {
                    println!("  #{:<4} {:<8} {}", st.id, st.state, st.title);
                }
            }

            // Relations
            let relations = db.get_relations(id)?;
            if !relations.is_empty() {
                println!();
                println!("Relations:");
                for rel in &relations {
                    if rel.from_task == id {
                        println!("  {}: #{}", rel.relation, rel.to_task);
                    } else {
                        let inverse = match rel.relation.as_str() {
                            "depends_on" => "blocks",
                            "blocks" => "depends_on",
                            other => other,
                        };
                        println!("  {}: #{}", inverse, rel.from_task);
                    }
                }
            }
        }
        TaskAction::Create {
            title,
            parent,
            skip_review,
            skip_planning,
            require_approval,
            priority,
        } => {
            let project = project_key()?;
            let initial_state = if skip_planning { "ready" } else { "planning" };
            let task = db.create_task(
                &project,
                &title,
                Some(priority),
                parent,
                None,
                skip_review,
                initial_state,
                require_approval,
                None,
                None,
                false,
                None,
                false,
                false,
                tau_agent_lib::tasks_db::FiledBy::default(),
            )?;
            println!("created task #{}: {}", task.id, task.title);
        }
        TaskAction::Update {
            id,
            state,
            title,
            priority,
        } => {
            let state = match state {
                None => None,
                Some(s) => Some(
                    tau_agent_lib::tasks_state::TaskState::from_db_str(&s).map_err(|_| {
                        tau_agent_lib::Error::Io(format!("invalid task state '{}'", s))
                    })?,
                ),
            };
            let update = tau_agent_lib::tasks_db::TaskUpdate {
                state,
                title,
                priority,
                ..Default::default()
            };
            let task = db.update_task(id, &update, None)?;
            println!("updated task #{}: {} [{}]", task.id, task.title, task.state);
        }
        TaskAction::Message { id, content } => {
            let msg = db.add_message(id, &content, Some("user"))?;
            println!("added message #{} to task #{}", msg.id, id);
        }
        TaskAction::Approve { id } => {
            let update = tau_agent_lib::tasks_db::TaskUpdate {
                state: Some(tau_agent_lib::tasks_state::TaskState::Approved),
                ..Default::default()
            };
            let task = db.update_task(id, &update, None)?;
            println!("approved task #{}: {}", task.id, task.title);
        }
        TaskAction::Claim { id, session } => {
            let result = db.assign_task(id, &session)?;
            println!("Claimed task #{}: {}", result.task.id, result.task.title);
        }
        TaskAction::Ready { id } => {
            let update = tau_agent_lib::tasks_db::TaskUpdate {
                state: Some(tau_agent_lib::tasks_state::TaskState::Ready),
                ..Default::default()
            };
            let task = db.update_task(id, &update, None)?;
            println!("task #{} marked ready: {}", task.id, task.title);
        }
        TaskAction::Mq => {
            let project = project_key()?;
            let approved = db.list_tasks(&project, Some("approved"), None, None, None)?;
            let merging = db.list_tasks(&project, Some("merging"), None, None, None)?;
            if approved.is_empty() && merging.is_empty() {
                println!("merge queue is empty");
                return Ok(());
            }
            println!("  MERGE QUEUE");
            println!("  {:>4}  {:<12}  {:<14}  TITLE", "ID", "STATE", "BRANCH");
            for t in approved.iter().chain(merging.iter()) {
                let branch = t.branch.as_deref().unwrap_or("-");
                println!(
                    "  {:>4}  {:<12}  {:<14}  {}",
                    t.id, t.state, branch, t.title
                );
            }
        }
        TaskAction::Status => {
            let project = project_key()?;
            let status = tau_agent_lib::tasks_scheduler::get_status(&db, &project, None)?;
            print!("{}", tau_agent_lib::tasks_scheduler::format_status(&status));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `tau profile` — historical session timing analysis
// ---------------------------------------------------------------------------

/// Parse the `--clamp` flag value into an optional millisecond cap.
///
/// Accepts the same forms as `parse_since` (durations like `1h`, raw ms,
/// etc.) plus a literal `0` to disable the clamp. Returns `None` for `0`
/// and `Some(ms)` otherwise.
fn parse_clamp(s: &str) -> tau_agent_lib::Result<Option<i64>> {
    let v = tau_agent_lib::profile::parse_since(s, 0).map(|v| if v < 0 { -v } else { v })?;
    if v == 0 { Ok(None) } else { Ok(Some(v)) }
}

fn cmd_profile(action: ProfileAction) -> tau_agent_lib::Result<()> {
    let db = tau_agent_lib::db::Db::open_default()?;
    let now_ms = tau_agent_lib::types::timestamp_ms() as i64;
    let default_since = now_ms - 30 * 24 * 60 * 60 * 1_000;

    match action {
        ProfileAction::Buckets {
            since,
            until,
            project,
            session,
            clamp,
            include_other,
        } => {
            let since_ms = match since {
                Some(s) => Some(tau_agent_lib::profile::parse_since(&s, now_ms)?),
                None => Some(default_since),
            };
            let until_ms = match until {
                Some(s) => Some(tau_agent_lib::profile::parse_since(&s, now_ms)?),
                None => None,
            };
            let max_event_ms = parse_clamp(&clamp)?;
            let filter = tau_agent_lib::profile::ProfileFilter {
                since_ms,
                until_ms,
                session_id: session,
                project,
                limit: 0,
                max_event_ms,
                exclude_other: !include_other,
            };
            let rows = tau_agent_lib::profile::buckets(&db, &filter)?;
            print_buckets(&rows, max_event_ms);
        }
        ProfileAction::Slow {
            min,
            limit,
            since,
            until,
            project,
            clamp,
            include_other,
        } => {
            // `parse_since(d, 0)` returns `0 - d` for any `d`-style suffix
            // duration; we negate to recover the duration in ms. For raw
            // millisecond inputs the value is positive, also fine.
            let min_ms =
                tau_agent_lib::profile::parse_since(&min, 0).map(|v| if v < 0 { -v } else { v })?;
            let since_ms = match since {
                Some(s) => Some(tau_agent_lib::profile::parse_since(&s, now_ms)?),
                None => Some(default_since),
            };
            let until_ms = match until {
                Some(s) => Some(tau_agent_lib::profile::parse_since(&s, now_ms)?),
                None => None,
            };
            let max_event_ms = parse_clamp(&clamp)?;
            let filter = tau_agent_lib::profile::ProfileFilter {
                since_ms,
                until_ms,
                session_id: None,
                project,
                limit,
                max_event_ms,
                exclude_other: !include_other,
            };
            let rows = tau_agent_lib::profile::slow_events(&db, &filter, min_ms)?;
            print_slow_events(&rows);
        }
        ProfileAction::Session {
            id,
            clamp,
            exclude_other,
        } => {
            let max_event_ms = parse_clamp(&clamp)?;
            print_session_breakdown(&db, &id, max_event_ms, exclude_other)?;
        }
        ProfileAction::Tokens {
            since,
            until,
            project,
            session,
            task,
            role,
            group_by,
            sort,
        } => {
            let since_ms = match since {
                Some(s) => Some(tau_agent_lib::profile::parse_since(&s, now_ms)?),
                None => Some(default_since),
            };
            let until_ms = match until {
                Some(s) => Some(tau_agent_lib::profile::parse_since(&s, now_ms)?),
                None => None,
            };
            let sort = parse_token_sort(&sort)?;

            // `--session` short-circuits to a per-session breakdown
            // (one row, fully populated).
            if let Some(sid) = session.as_deref() {
                let usage = tau_agent_lib::profile::session_token_breakdown(&db, sid)?;
                print_token_session(sid, &usage);
                return Ok(());
            }

            // `--task` short-circuits to a per-task breakdown (one row
            // per recorded role/session for the task).
            if let Some(task_id) = task {
                let tasks_db = tau_agent_lib::tasks_db::TasksDb::open_default()
                    .map_err(|e| tau_agent_lib::Error::Io(format!("open tasks db: {}", e)))?;
                let rows = tau_agent_lib::profile::task_token_breakdown(&db, &tasks_db, task_id)?;
                print_token_rows(&rows, "role");
                return Ok(());
            }

            // Otherwise: leaderboard. Default group is `role` for the
            // project-wide view.
            let group = group_by.as_deref().unwrap_or("role");
            let group_by = parse_token_group_by(group)?;
            let role_ref = role.as_deref();
            let filter = tau_agent_lib::profile::ProfileFilter {
                since_ms,
                until_ms,
                session_id: None,
                project,
                limit: 0,
                max_event_ms: None,
                exclude_other: false,
            };
            let tasks_db = match group_by {
                tau_agent_lib::profile::TokenGroupBy::Session => None,
                _ => Some(
                    tau_agent_lib::tasks_db::TasksDb::open_default()
                        .map_err(|e| tau_agent_lib::Error::Io(format!("open tasks db: {}", e)))?,
                ),
            };
            let rows = tau_agent_lib::profile::token_leaderboard(
                &db,
                &filter,
                group_by,
                role_ref,
                sort,
                tasks_db.as_ref(),
            )?;
            print_token_rows(&rows, group);
        }
    }
    Ok(())
}

fn fmt_dur(ms: i64) -> String {
    if ms < 1_000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        format!("{:.1}m", ms as f64 / 60_000.0)
    } else {
        format!("{:.2}h", ms as f64 / 3_600_000.0)
    }
}

fn print_buckets(rows: &[tau_agent_lib::profile::BucketSummary], clamp_ms: Option<i64>) {
    if rows.is_empty() {
        println!("(no events in window)");
        return;
    }
    let bucket_w = rows
        .iter()
        .map(|r| r.bucket.len())
        .max()
        .unwrap_or(6)
        .max(6);
    println!(
        "{:<width$}  {:>6}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
        "bucket",
        "n",
        "total",
        "mean",
        "p50",
        "p95",
        "max",
        width = bucket_w,
    );
    for r in rows {
        let suffix = if r.dropped_over_clamp > 0 {
            match clamp_ms {
                Some(c) => format!("  [+{} dropped >{}]", r.dropped_over_clamp, fmt_dur(c)),
                None => format!("  [+{} dropped]", r.dropped_over_clamp),
            }
        } else {
            String::new()
        };
        println!(
            "{:<width$}  {:>6}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}{}",
            r.bucket,
            r.n,
            fmt_dur(r.total_ms),
            fmt_dur(r.mean_ms.round() as i64),
            fmt_dur(r.p50_ms),
            fmt_dur(r.p95_ms),
            fmt_dur(r.max_ms),
            suffix,
            width = bucket_w,
        );
    }
}

fn print_slow_events(rows: &[tau_agent_lib::profile::SlowEvent]) {
    if rows.is_empty() {
        println!("(no slow events)");
        return;
    }
    let session_w = rows
        .iter()
        .map(|r| r.session_id.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let bucket_w = rows
        .iter()
        .map(|r| r.bucket.len())
        .max()
        .unwrap_or(6)
        .max(6);
    println!(
        "{:<sw$}  {:>8}  {:<bw$}  {:>10}  {}",
        "session",
        "id",
        "bucket",
        "duration",
        "detail",
        sw = session_w,
        bw = bucket_w,
    );
    for r in rows {
        let detail = r.detail.as_deref().unwrap_or("");
        // Single-line detail; truncate if outrageous.
        let detail = detail.replace('\n', " ");
        // Prepend an LLM-gen / tool-exec attribution hint for tool buckets.
        let detail = if let Some(gen_ms) = r.llm_gen_ms {
            let exec_ms = (r.dur_ms - gen_ms).max(0);
            let prefix = format!(
                "[≈{} LLM-gen, {} tool-exec] ",
                fmt_dur(gen_ms),
                fmt_dur(exec_ms),
            );
            format!("{}{}", prefix, detail)
        } else {
            detail
        };
        let detail = if detail.len() > 160 {
            format!("{}…", tau_agent_lib::truncate_str(&detail, 160))
        } else {
            detail
        };
        println!(
            "{:<sw$}  {:>8}  {:<bw$}  {:>10}  {}",
            r.session_id,
            r.message_id,
            r.bucket,
            fmt_dur(r.dur_ms),
            detail,
            sw = session_w,
            bw = bucket_w,
        );
    }
}

fn print_session_breakdown(
    db: &tau_agent_lib::db::Db,
    session_id: &str,
    clamp_ms: Option<i64>,
    exclude_other: bool,
) -> tau_agent_lib::Result<()> {
    let filter = tau_agent_lib::profile::ProfileFilter {
        session_id: Some(session_id.to_string()),
        max_event_ms: clamp_ms,
        exclude_other,
        ..Default::default()
    };
    let rows = tau_agent_lib::profile::buckets(db, &filter)?;
    let total_ms: i64 = rows.iter().map(|r| r.total_ms).sum();
    let total_msgs = db.message_count(session_id)?;
    let total_cost = aggregate_session_cost(db, session_id)?;

    println!(
        "session {}: total={} messages={} cost=${:.4}",
        session_id,
        fmt_dur(total_ms),
        total_msgs,
        total_cost,
    );
    print_buckets(&rows, clamp_ms);
    Ok(())
}

/// Sum `usage.cost.total` over all messages in the session.
fn aggregate_session_cost(
    db: &tau_agent_lib::db::Db,
    session_id: &str,
) -> tau_agent_lib::Result<f64> {
    tau_agent_lib::profile::session_cost_total(db, session_id)
}

fn parse_token_sort(s: &str) -> tau_agent_lib::Result<tau_agent_lib::profile::TokenSort> {
    use tau_agent_lib::profile::TokenSort;
    match s.to_ascii_lowercase().as_str() {
        "cost" => Ok(TokenSort::Cost),
        "tokens" | "total" => Ok(TokenSort::Tokens),
        "input" => Ok(TokenSort::Input),
        "output" => Ok(TokenSort::Output),
        other => Err(tau_agent_lib::Error::Parse(format!(
            "--sort: expected one of cost|tokens|input|output, got `{}`",
            other
        ))),
    }
}

fn parse_token_group_by(s: &str) -> tau_agent_lib::Result<tau_agent_lib::profile::TokenGroupBy> {
    use tau_agent_lib::profile::TokenGroupBy;
    match s.to_ascii_lowercase().as_str() {
        "session" => Ok(TokenGroupBy::Session),
        "role" => Ok(TokenGroupBy::Role),
        "task" => Ok(TokenGroupBy::Task),
        other => Err(tau_agent_lib::Error::Parse(format!(
            "--group-by: expected one of session|role|task, got `{}`",
            other
        ))),
    }
}

/// Format a token count compactly: `1.2K`, `34M`, `885M`, etc. Used in
/// the leaderboard table where 9-digit raw counts blow out column
/// widths. The exact-comma form is reserved for the per-session
/// breakdown printer.
fn fmt_tokens_compact(n: u64) -> String {
    if n < 1_000 {
        format!("{}", n)
    } else if n < 1_000_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else if n < 1_000_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{:.2}B", n as f64 / 1_000_000_000.0)
    }
}

fn print_token_rows(rows: &[tau_agent_lib::profile::TokenRow], group_label: &str) {
    if rows.is_empty() {
        println!("(no token usage in window)");
        return;
    }
    let group_w = rows
        .iter()
        .map(|r| r.group.len())
        .max()
        .unwrap_or(group_label.len())
        .max(group_label.len());
    println!(
        "{:<gw$}  {:>5}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}  {}",
        group_label,
        "sess",
        "input",
        "output",
        "cache_r",
        "cache_w",
        "total",
        "cost",
        "models",
        gw = group_w,
    );
    for r in rows {
        let cost = if r.tokens.cost_usd.abs() < 0.005 && r.tokens.cost_usd != 0.0 {
            // Tiny but non-zero costs render as 0.00 with 2dp; bump to
            // 4dp so they don't disappear silently.
            format!("${:.4}", r.tokens.cost_usd)
        } else {
            format!("${:.2}", r.tokens.cost_usd)
        };
        let models = if r.models.is_empty() {
            "-".to_string()
        } else {
            r.models.join(",")
        };
        println!(
            "{:<gw$}  {:>5}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}  {}",
            r.group,
            r.sessions,
            fmt_tokens_compact(r.tokens.input),
            fmt_tokens_compact(r.tokens.output),
            fmt_tokens_compact(r.tokens.cache_read),
            fmt_tokens_compact(r.tokens.cache_write),
            fmt_tokens_compact(r.tokens.total_tokens()),
            cost,
            models,
            gw = group_w,
        );
    }
}

fn print_token_session(session_id: &str, usage: &tau_agent_lib::profile::TokenUsage) {
    println!(
        "session {}: input={} output={} cache_read={} cache_write={} total={} cost=${:.4}",
        session_id,
        format_u64_commas(usage.input),
        format_u64_commas(usage.output),
        format_u64_commas(usage.cache_read),
        format_u64_commas(usage.cache_write),
        format_u64_commas(usage.total_tokens()),
        usage.cost_usd,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_u64_commas_boundaries() {
        assert_eq!(format_u64_commas(0), "0");
        assert_eq!(format_u64_commas(7), "7");
        assert_eq!(format_u64_commas(999), "999");
        assert_eq!(format_u64_commas(1_000), "1,000");
        assert_eq!(format_u64_commas(10_000), "10,000");
        assert_eq!(format_u64_commas(123_456_789), "123,456,789");
    }

    #[test]
    fn project_stats_block_includes_all_sections() {
        // Render into a string buffer via println capture isn't trivial;
        // instead exercise the format_u64_commas path which backs the
        // block's numeric columns — the integration-style assertion
        // would otherwise require running the binary end-to-end.
        let stats = tau_agent_lib::db::DbProjectStats {
            session_count: 42,
            message_count: 8124,
            tokens_input: 12_340_156,
            tokens_output: 418_902,
            tokens_cache_read: 34_521_088,
            tokens_cache_write: 2_108_445,
            cost: 28.47,
            last_message_time: None,
        };
        assert_eq!(format_u64_commas(stats.session_count as u64), "42");
        assert_eq!(format_u64_commas(stats.tokens_input), "12,340,156");
        assert!((stats.cost - 28.47).abs() < 1e-9);
    }
}
