mod completer;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::ArgValueCandidates;

#[derive(Parser)]
#[command(name = "tau", about = "LLM agent CLI", infer_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
        /// Model name (default: saved setting or claude-sonnet-4-6)
        #[arg(long, add = ArgValueCandidates::new(completer::model_completer))]
        model: Option<String>,
        /// Disable TUI (use plain text streaming)
        #[arg(long)]
        no_tui: bool,
        /// Max child sessions this session can spawn (0 = no children)
        #[arg(long, default_value = "16")]
        child_budget: u32,
    },
    /// Log in to an LLM provider (OAuth)
    Login {
        /// Provider name
        #[arg(default_value = "anthropic")]
        provider: String,
    },
    /// Tool execution worker - sync/sequential (internal, used by daemon)
    #[command(hide = true)]
    Worker,
    /// Async tool execution worker - default (internal, used by daemon)
    #[command(hide = true)]
    Worker2,
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

fn main() {
    clap_complete::env::CompleteEnv::with_factory(Cli::command).complete();
    let cli = Cli::parse();

    smol::block_on(async {
        if let Err(e) = run(cli).await {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    });
}

async fn run(cli: Cli) -> tau::Result<()> {
    match cli.command {
        Commands::Chat {
            message,
            session,
            model,
            no_tui,
            child_budget,
        } => {
            // Resolve model: CLI flag > saved setting > hardcoded default
            let model = model.unwrap_or_else(|| {
                tau_tui::settings::load()
                    .tui
                    .model
                    .unwrap_or_else(|| "claude-sonnet-4-6".into())
            });
            cmd_chat(message, session, &model, no_tui, child_budget).await?;
        }
        Commands::Worker => {
            tau::worker::run_worker_loop();
            return Ok(());
        }
        Commands::Worker2 => {
            tau::worker2::run();
            return Ok(());
        }
        Commands::PluginTasks => {
            tau::tasks::run_tasks_plugin();
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
                let auth = tau::auth::AuthStorage::open_default();
                auth.remove(&provider)?;
                eprintln!("logged out from {}", provider);
            }
        },
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cumulative usage tracking
// ---------------------------------------------------------------------------

#[derive(Default)]
struct UsageTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    /// Context window size from model.
    context_window: u64,
    /// Context tokens from last successful response.
    context_tokens: Option<u64>,
    /// Whether using OAuth subscription.
    is_subscription: bool,
}

impl UsageTotals {
    fn add(&mut self, usage: &tau::Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.cost += usage.cost.total;
        // Context estimate: last response's total input (fresh + cached)
        self.context_tokens = Some(usage.input + usage.cache_read + usage.cache_write);
    }

    fn display(&self) {
        use tau::protocol::format_tokens;
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

async fn cmd_login(provider: &str) -> tau::Result<()> {
    match provider {
        "anthropic" => {
            eprintln!("Logging in to Anthropic (OAuth)...");
            let creds = smol::unblock(tau::auth::login_anthropic).await?;
            let auth = tau::auth::AuthStorage::open_default();
            auth.set("anthropic", tau::auth::AuthCredential::Oauth(creds))?;
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
                .map_err(|e| tau::Error::Io(e.to_string()))?;
            let key = key.trim().to_string();
            if key.is_empty() {
                return Err(tau::Error::Io("empty API key".into()));
            }
            let auth = tau::auth::AuthStorage::open_default();
            auth.set(provider, tau::auth::AuthCredential::ApiKey { key })?;
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
) -> tau::Result<()> {
    let mut client = tau::client::Client::connect_or_start().await?;

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
        client
            .send(&tau::protocol::Request::CreateSession {
                model: Some(model_id),
                provider,
                system_prompt: None,
                cwd,
                parent_id: None,
                child_budget,
                tagline: None,
                auto_archive: false,
            })
            .await?;

        let mut created_id = None;
        client
            .recv_streaming(|resp| {
                if let tau::protocol::Response::SessionCreated { session_id } = resp {
                    created_id = Some(session_id.clone());
                }
            })
            .await?;
        let id = created_id.ok_or_else(|| tau::Error::Io("failed to create session".into()))?;
        (id, false)
    };

    // Get session info for display; error if user specified a session that doesn't exist
    let info = if session_id_user_provided {
        Some(get_session_info(&mut client, &session_id).await?)
    } else {
        get_session_info(&mut client, &session_id).await.ok()
    };
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
        send_and_print(&mut client, &session_id, &text, &mut totals).await?;
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
        interactive_loop(&mut client, session_id, &mut totals).await?;
    } else {
        // TUI mode
        tau_tui::run(
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
    client: &mut tau::client::Client,
    session_id: &str,
) -> tau::Result<tau::protocol::SessionInfo> {
    client
        .send(&tau::protocol::Request::GetSessionInfo {
            session_id: session_id.to_string(),
        })
        .await?;

    let mut info = None;
    let mut error = None;
    client
        .recv_streaming(|resp| match resp {
            tau::protocol::Response::SessionInfo { info: i } => {
                info = Some(i.clone());
            }
            tau::protocol::Response::Error { message } => {
                error = Some(message.clone());
            }
            _ => {}
        })
        .await?;

    match (info, error) {
        (Some(i), _) => Ok(i),
        (_, Some(e)) => Err(tau::Error::Io(e)),
        _ => Err(tau::Error::Io("no response".into())),
    }
}

/// Create a new session and return its ID.
async fn cli_create_session(
    client: &mut tau::client::Client,
    model: Option<String>,
    cwd: Option<String>,
    parent_id: Option<String>,
) -> tau::Result<String> {
    client
        .send(&tau::protocol::Request::CreateSession {
            model,
            provider: None,
            system_prompt: None,
            cwd,
            parent_id,
            child_budget: 0,
            tagline: None,
            auto_archive: false,
        })
        .await?;

    let mut created_id = None;
    client
        .recv_streaming(|resp| {
            if let tau::protocol::Response::SessionCreated { session_id } = resp {
                created_id = Some(session_id.clone());
            }
        })
        .await?;

    created_id.ok_or_else(|| tau::Error::Io("failed to create session".into()))
}

async fn send_and_print(
    client: &mut tau::client::Client,
    session_id: &str,
    text: &str,
    totals: &mut UsageTotals,
) -> tau::Result<()> {
    client
        .send(&tau::protocol::Request::Chat {
            session_id: session_id.to_string(),
            text: text.to_string(),
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
                                let cancel_req = tau::protocol::Request::CancelChat {
                                    session_id: session_id_clone.clone(),
                                };
                                if let Ok(stream) = std::os::unix::net::UnixStream::connect(
                                    tau::server::socket_path(),
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
            tau::protocol::Response::Stream { event } => {
                match event.as_ref() {
                    tau::StreamEvent::TextDelta { delta, .. } => {
                        print!("{}", delta);
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                    }
                    tau::StreamEvent::ToolcallEnd { tool_call, .. } => {
                        let args_str = tool_call.arguments.to_string();
                        let preview = if args_str.len() > 100 {
                            format!("{}...", &args_str[..100])
                        } else {
                            args_str
                        };
                        eprintln!("[tool: {} {}]", tool_call.name, preview);
                    }
                    tau::StreamEvent::ToolOutputDelta { .. } => {
                        eprint!("."); // progress dot for streaming output
                    }
                    tau::StreamEvent::ToolResult {
                        tool_name,
                        is_error,
                        content,
                        ..
                    } => {
                        let preview: String =
                            content.split_whitespace().collect::<Vec<_>>().join(" ");
                        let preview = if preview.len() > 100 {
                            format!("{}...", &preview[..100])
                        } else {
                            preview
                        };
                        if *is_error {
                            eprintln!("[tool error: {} {}]", tool_name, preview);
                        } else {
                            eprintln!("[tool ok: {} {}]", tool_name, preview);
                        }
                    }
                    tau::StreamEvent::Done { message, .. } => {
                        // Only print newline if there was text content
                        if message.content.iter().any(
                            |c| matches!(c, tau::AssistantContent::Text(t) if !t.text.is_empty()),
                        ) {
                            println!();
                        }
                        totals.add(&message.usage);
                    }
                    tau::StreamEvent::Error { error, .. } => {
                        if let Some(ref msg) = error.error_message {
                            eprintln!("\nerror: {}", msg);
                        }
                    }
                    tau::StreamEvent::Status { message } => {
                        eprintln!("[{}]", message);
                    }
                    _ => {}
                }
            }
            tau::protocol::Response::AgentDone => {
                totals.display();
            }
            tau::protocol::Response::Cancelled => {
                was_cancelled = true;
                eprintln!("[cancelled]");
                totals.display();
            }
            tau::protocol::Response::ServerShutdown { restart } => {
                if *restart {
                    eprintln!("[server restarting...]");
                } else {
                    eprintln!("[server shutting down]");
                }
            }
            tau::protocol::Response::Error { message } => {
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
    client: &mut tau::client::Client,
    mut session_id: String,
    totals: &mut UsageTotals,
) -> tau::Result<()> {
    let hist = history_path();
    if let Some(parent) = hist.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let mut rl = rustyline::DefaultEditor::new()
        .map_err(|e| tau::Error::Io(format!("readline init: {}", e)))?;
    let _ = rl.load_history(&hist);

    loop {
        let line = match rl.readline("tau> ") {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(tau::Error::Io(format!("readline: {}", e))),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        rl.add_history_entry(line)
            .map_err(|e| tau::Error::Io(format!("history: {}", e)))?;
        let _ = rl.save_history(&hist);

        // Handle slash commands
        if line.starts_with('/') {
            match handle_slash_command(client, &mut session_id, line, totals).await {
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

        match send_and_print(client, &session_id, line, totals).await {
            Ok(()) => {}
            Err(e) => {
                if try_reconnect(client, &e).await {
                    // Retry the message after reconnecting
                    if let Err(e) = send_and_print(client, &session_id, line, totals).await {
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
async fn try_reconnect(client: &mut tau::client::Client, err: &tau::Error) -> bool {
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
        if let Ok(new_client) = tau::client::Client::connect_or_start().await {
            *client = new_client;
            eprintln!("[reconnected]");
            return true;
        }
    }
    eprintln!("[reconnection failed]");
    false
}

fn pct(b: Option<&tau::auth::UsageBucket>) -> String {
    tau::protocol::format_utilization(b.and_then(|b| b.utilization))
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

async fn print_subscription_usage(client: &mut tau::client::Client) {
    client
        .send(&tau::protocol::Request::GetSubscriptionUsage)
        .await
        .ok();

    client
        .recv_streaming(|resp| {
            if let tau::protocol::Response::SubscriptionUsage { usage } = resp {
                fn bucket_line(label: &str, indent: bool, b: Option<&tau::auth::UsageBucket>) {
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
            } else if let tau::protocol::Response::Error { message } = resp {
                eprintln!("usage:    unavailable ({})", message);
            }
        })
        .await
        .ok();
}

/// Handle a slash command. Returns Ok(true) if the loop should exit.
async fn handle_slash_command(
    client: &mut tau::client::Client,
    session_id: &mut String,
    line: &str,
    totals: &mut UsageTotals,
) -> tau::Result<bool> {
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
            println!("tokens:   {}", tau::protocol::format_stats(&info.stats));

            if info.stats.is_subscription {
                print_subscription_usage(client).await;
            }
        }

        "/model" | "/models" => {
            if args.is_empty() {
                // Get current model first, then list
                let current_info = get_session_info(client, session_id).await.ok();
                let current_model_id = current_info.map(|i| i.model);

                client.send(&tau::protocol::Request::ListModels).await?;
                client
                    .recv_streaming(|resp| {
                        if let tau::protocol::Response::Models { models } = resp {
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
            } else {
                // Set model
                client
                    .send(&tau::protocol::Request::SetModel {
                        session_id: session_id.to_string(),
                        model_id: args.to_string(),
                    })
                    .await?;
                client
                    .recv_streaming(|resp| match resp {
                        tau::protocol::Response::ModelChanged { model } => {
                            eprintln!("model changed to {}", model.id);
                        }
                        tau::protocol::Response::Error { message } => {
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
                        .send(&tau::protocol::Request::SetCwd {
                            session_id: session_id.to_string(),
                            cwd: new_cwd.clone(),
                        })
                        .await?;
                    client.recv_streaming(|_| {}).await?;
                    // Notify the model about the cwd change
                    let notice = format!("[Working directory changed to: {}]", new_cwd);
                    send_and_print(client, session_id, &notice, totals).await?;
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
                .send(&tau::protocol::Request::ReloadPlugins {
                    session_id: session_id.to_string(),
                })
                .await?;
            client.recv_streaming(|_| {}).await?;
            eprintln!("Plugins reloaded");
        }

        "/fork" => {
            // Create a new session inheriting model/cwd from the current session
            let info = get_session_info(client, session_id).await?;
            let new_id =
                cli_create_session(client, Some(info.model), info.cwd, Some(session_id.clone()))
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
            let new_id = cli_create_session(client, None, cwd, None).await?;
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

async fn cmd_server_start(foreground: bool) -> tau::Result<()> {
    if tau::server::is_running() {
        eprintln!("server already running");
        return Ok(());
    }

    if foreground {
        tau::server::run().await?;
    } else {
        spawn_server_daemon()?;
    }
    Ok(())
}

fn spawn_server_daemon() -> tau::Result<()> {
    let exe = std::env::current_exe().map_err(|e| tau::Error::Io(e.to_string()))?;
    let child = std::process::Command::new(exe)
        .args(["server", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| tau::Error::Io(format!("spawn: {}", e)))?;
    eprintln!("server started (pid {})", child.id());

    // Wait for ready
    smol::block_on(async {
        for _ in 0..50 {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            if tau::server::is_running() {
                eprintln!("server ready at {}", tau::server::socket_path().display());
                return;
            }
        }
        eprintln!("warning: server may not have started");
    });
    Ok(())
}

async fn cmd_server_stop() -> tau::Result<()> {
    if !tau::server::is_running() {
        eprintln!("server not running");
        return Ok(());
    }
    let mut client = tau::client::Client::connect().await?;
    client
        .send(&tau::protocol::Request::Shutdown { restart: false })
        .await?;
    client.recv_streaming(|_| {}).await?;
    eprintln!("server stopped");
    Ok(())
}

async fn cmd_server_restart() -> tau::Result<()> {
    if tau::server::is_running() {
        let mut client = tau::client::Client::connect().await?;
        client
            .send(&tau::protocol::Request::Shutdown { restart: true })
            .await?;
        client.recv_streaming(|_| {}).await?;
        eprintln!("shutdown requested, waiting for server to exit...");
        // Wait up to 65s (server drains for up to 60s)
        for i in 0..650 {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            if !tau::server::is_running() {
                break;
            }
            if i > 0 && i % 50 == 0 {
                eprintln!("still waiting... ({}s)", i / 10);
            }
        }
        if tau::server::is_running() {
            return Err(tau::Error::Io("old server didn't exit within 65s".into()));
        }
    }
    spawn_server_daemon()?;
    Ok(())
}

fn cmd_server_status() {
    if tau::server::is_running() {
        eprintln!("server running at {}", tau::server::socket_path().display());
    } else {
        eprintln!("server not running");
    }
}

async fn cmd_auth_status() -> tau::Result<()> {
    let auth = tau::auth::AuthStorage::open_default();
    let providers = auth.list()?;
    if providers.is_empty() {
        println!("not logged in to any providers");
        println!("run `tau login` to authenticate");
    } else {
        for p in &providers {
            let status = match auth.get(p)? {
                Some(tau::auth::AuthCredential::Oauth(creds)) => {
                    if creds.is_expired() {
                        "oauth (expired, will auto-refresh)"
                    } else {
                        "oauth (valid)"
                    }
                }
                Some(tau::auth::AuthCredential::ApiKey { .. }) => "api key",
                None => "none",
            };
            println!("{}\t{}", p, status);
        }
    }
    Ok(())
}

async fn cmd_sessions_list(include_archived: bool) -> tau::Result<()> {
    let mut client = tau::client::Client::connect_or_start().await?;
    client
        .send(&tau::protocol::Request::ListSessions { include_archived })
        .await?;

    client
        .recv_streaming(|resp| {
            if let tau::protocol::Response::Sessions { sessions } = resp {
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
    session: &tau::protocol::SessionInfo,
    all: &[tau::protocol::SessionInfo],
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    let stats = tau::protocol::format_stats(&session.stats);
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

async fn cmd_sessions_archive(id: &str) -> tau::Result<()> {
    let mut client = tau::client::Client::connect_or_start().await?;
    client
        .send(&tau::protocol::Request::ArchiveSession {
            session_id: id.to_string(),
            require_ancestor: None,
        })
        .await?;
    client.recv_streaming(|_| {}).await?;
    eprintln!("archived session {}", id);
    Ok(())
}

async fn cmd_sessions_delete(id: &str) -> tau::Result<()> {
    let mut client = tau::client::Client::connect_or_start().await?;
    client
        .send(&tau::protocol::Request::DeleteSession {
            session_id: id.to_string(),
        })
        .await?;
    client.recv_streaming(|_| {}).await?;
    eprintln!("deleted session {}", id);
    Ok(())
}

fn cmd_sessions_dump(id: &str, output: Option<&str>) -> tau::Result<()> {
    let db = tau::db::Db::open_default()?;
    let recording = tau::replay::dump_session(&db, id)?;
    let json = serde_json::to_string_pretty(&recording)
        .map_err(|e| tau::Error::Io(format!("serialize recording: {}", e)))?;

    if let Some(path) = output {
        std::fs::write(path, &json)
            .map_err(|e| tau::Error::Io(format!("write {}: {}", path, e)))?;
        eprintln!("dumped session {} to {}", id, path);
    } else {
        println!("{}", json);
    }
    Ok(())
}

async fn cmd_sessions_gc(older_than: u64) -> tau::Result<()> {
    let mut client = tau::client::Client::connect_or_start().await?;
    client
        .send(&tau::protocol::Request::GcSessions {
            older_than_days: older_than,
        })
        .await?;

    client
        .recv_streaming(|resp| match resp {
            tau::protocol::Response::GcComplete { deleted } => {
                eprintln!("gc: deleted {} archived session(s)", deleted);
            }
            tau::protocol::Response::Error { message } => {
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

fn cmd_providers_list() -> tau::Result<()> {
    let cfg = tau::config::load_config()?;
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
) -> tau::Result<()> {
    let mut cfg = tau::config::load_config()?;
    cfg.providers.insert(
        name.to_string(),
        tau::config::ProviderConfig {
            api: api.to_string(),
            base_url: base_url.to_string(),
            api_key: api_key.map(String::from),
            models: Vec::new(),
        },
    );
    tau::config::save_config(&cfg)?;
    eprintln!("provider '{}' added. Restart server to apply.", name);
    Ok(())
}

fn cmd_providers_remove(name: &str) -> tau::Result<()> {
    let mut cfg = tau::config::load_config()?;
    if cfg.providers.remove(name).is_none() {
        eprintln!("provider '{}' not found in config", name);
        return Ok(());
    }
    tau::config::save_config(&cfg)?;
    eprintln!("provider '{}' removed. Restart server to apply.", name);
    Ok(())
}

fn parse_thinking_style(s: &str) -> tau::Result<tau::ThinkingStyle> {
    match s {
        "none" => Ok(tau::ThinkingStyle::None),
        "anthropic" => Ok(tau::ThinkingStyle::Anthropic),
        "openai" => Ok(tau::ThinkingStyle::OpenAi),
        "qwen" => Ok(tau::ThinkingStyle::Qwen),
        _ => Err(tau::Error::Parse(format!(
            "unknown thinking style: '{}'. Use: none, anthropic, openai, qwen",
            s
        ))),
    }
}

fn cmd_models_list() -> tau::Result<()> {
    let cfg = tau::config::load_config()?;
    let models = tau::config::resolve_models(&cfg);
    for m in &models {
        let thinking = match m.thinking {
            tau::ThinkingStyle::None => "",
            tau::ThinkingStyle::Anthropic => " [anthropic]",
            tau::ThinkingStyle::OpenAi => " [openai]",
            tau::ThinkingStyle::Qwen => " [qwen]",
        };
        println!(
            "  {:<32} {:<12} {}K ctx{}",
            m.id,
            m.provider,
            m.context_window / 1000,
            thinking,
        );
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
) -> tau::Result<()> {
    let thinking = parse_thinking_style(thinking)?;
    let mut cfg = tau::config::load_config()?;
    let pc = cfg.providers.get_mut(provider).ok_or_else(|| {
        tau::Error::Io(format!(
            "provider '{}' not found. Add it first with `tau providers add`.",
            provider
        ))
    })?;
    // Remove existing model with same id
    pc.models.retain(|m| m.id != id);
    pc.models.push(tau::config::ModelConfig {
        id: id.to_string(),
        name: name.map(String::from),
        context_window: context,
        max_tokens,
        thinking,
        cost: tau::ModelCost::default(),
    });
    tau::config::save_config(&cfg)?;
    eprintln!(
        "model '{}' added to provider '{}'. Restart server to apply.",
        id, provider
    );
    Ok(())
}

fn cmd_models_remove(id: &str, provider: &str) -> tau::Result<()> {
    let mut cfg = tau::config::load_config()?;
    let pc = cfg
        .providers
        .get_mut(provider)
        .ok_or_else(|| tau::Error::Io(format!("provider '{}' not found", provider)))?;
    let before = pc.models.len();
    pc.models.retain(|m| m.id != id);
    if pc.models.len() == before {
        eprintln!("model '{}' not found in provider '{}'", id, provider);
        return Ok(());
    }
    tau::config::save_config(&cfg)?;
    eprintln!(
        "model '{}' removed from provider '{}'. Restart server to apply.",
        id, provider
    );
    Ok(())
}

fn format_time_ago(unix_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
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
