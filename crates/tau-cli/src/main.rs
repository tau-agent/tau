use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tau", about = "LLM agent CLI")]
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
        #[arg(short, long)]
        session: Option<String>,
        /// Model name
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
    },
    /// Log in to an LLM provider (OAuth)
    Login {
        /// Provider name
        #[arg(default_value = "anthropic")]
        provider: String,
    },
    /// Manage the tau server
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Manage sessions
    Sessions {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Manage authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the server
    Start {
        /// Run in foreground (don't daemonize)
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the server
    Stop,
    /// Restart the server
    Restart,
    /// Check server status
    Status,
}

#[derive(Subcommand)]
enum SessionAction {
    /// List all sessions
    List,
    /// Delete a session
    Delete {
        /// Session ID
        id: String,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Show authentication status
    Status,
    /// Log out from a provider
    Logout {
        /// Provider name
        provider: String,
    },
}

fn main() {
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
            model: _model,
        } => {
            cmd_chat(message, session).await?;
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
            SessionAction::List => {
                cmd_sessions_list().await?;
            }
            SessionAction::Delete { id } => {
                cmd_sessions_delete(&id).await?;
            }
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
    eprintln!("Logging in to {}...", provider);

    let provider = provider.to_string();
    let creds = smol::unblock(move || {
        if provider == "anthropic" {
            tau::auth::login_anthropic()
        } else {
            Err(tau::Error::Io(format!(
                "unknown OAuth provider: {}",
                provider
            )))
        }
    })
    .await?;

    let auth = tau::auth::AuthStorage::open_default();
    auth.set("anthropic", tau::auth::AuthCredential::Oauth(creds))?;
    eprintln!("Login successful! Credentials saved.");
    Ok(())
}

async fn cmd_chat(message: Option<String>, session_id: Option<String>) -> tau::Result<()> {
    let mut client = tau::client::Client::connect_or_start().await?;

    let session_id = if let Some(id) = session_id {
        id
    } else {
        client
            .send(&tau::protocol::Request::CreateSession {
                model: None,
                provider: None,
                system_prompt: None,
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
        created_id.ok_or_else(|| tau::Error::Io("failed to create session".into()))?
    };

    // Initialize totals with session info (for context_window, is_subscription)
    let mut totals = UsageTotals::default();
    if let Ok(info) = get_session_info(&mut client, &session_id).await {
        totals.context_window = info.stats.context_window;
        totals.is_subscription = info.stats.is_subscription;
        // If resuming an existing session, seed totals from stored stats
        totals.input = info.stats.tokens.input;
        totals.output = info.stats.tokens.output;
        totals.cache_read = info.stats.tokens.cache_read;
        totals.cache_write = info.stats.tokens.cache_write;
        totals.cost = info.stats.cost;
        totals.context_tokens = info.stats.context_tokens;
    }

    if let Some(text) = message {
        send_and_print(&mut client, &session_id, &text, &mut totals).await?;
    } else {
        interactive_loop(&mut client, &session_id, &mut totals).await?;
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

    client
        .recv_streaming(|resp| match resp {
            tau::protocol::Response::Stream { event } => match event.as_ref() {
                tau::StreamEvent::TextDelta { delta, .. } => {
                    print!("{}", delta);
                }
                tau::StreamEvent::Done { message, .. } => {
                    println!();
                    totals.add(&message.usage);
                    totals.display();
                }
                tau::StreamEvent::Error { error, .. } => {
                    if let Some(ref msg) = error.error_message {
                        eprintln!("\nerror: {}", msg);
                    }
                }
                _ => {}
            },
            tau::protocol::Response::Error { message } => {
                eprintln!("error: {}", message);
            }
            _ => {}
        })
        .await?;

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
    session_id: &str,
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
            match handle_slash_command(client, session_id, line, totals).await {
                Ok(true) => break, // /quit
                Ok(false) => continue,
                Err(e) => {
                    eprintln!("error: {}", e);
                    continue;
                }
            }
        }

        send_and_print(client, session_id, line, totals).await?;
    }

    Ok(())
}

async fn print_subscription_usage(client: &mut tau::client::Client) {
    client
        .send(&tau::protocol::Request::GetSubscriptionUsage)
        .await
        .ok();

    client
        .recv_streaming(|resp| {
            if let tau::protocol::Response::SubscriptionUsage { usage } = resp {
                fn pct(b: Option<&tau::auth::UsageBucket>) -> String {
                    match b.and_then(|b| b.utilization) {
                        Some(u) => format!("{:.0}%", u * 100.0),
                        None => "?".into(),
                    }
                }
                fn resets(b: Option<&tau::auth::UsageBucket>) -> String {
                    b.and_then(|b| b.resets_at.as_deref())
                        .unwrap_or("?")
                        .to_string()
                }
                let fh = usage.five_hour.as_ref();
                let sd = usage.seven_day.as_ref();
                println!(
                    "usage:    5h {}/100%  7d {}/100%  resets {}",
                    pct(fh),
                    pct(sd),
                    resets(fh)
                );
                if let (Some(sonnet), Some(opus)) = (&usage.seven_day_sonnet, &usage.seven_day_opus)
                {
                    println!(
                        "          sonnet 7d {}  opus 7d {}",
                        pct(Some(sonnet)),
                        pct(Some(opus))
                    );
                }
                if usage.extra_usage_enabled
                    && let (Some(used), Some(limit)) = (
                        usage.extra_usage_used_credits,
                        usage.extra_usage_monthly_limit,
                    )
                {
                    println!("          extra: ${:.2}/${:.2} used", used, limit);
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
    session_id: &str,
    line: &str,
    _totals: &UsageTotals,
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

        "/help" => {
            println!("commands:");
            println!("  /status        show session info and stats");
            println!("  /model         list available models");
            println!("  /model <id>    switch to a different model");
            println!("  /help          show this help");
            println!("  /quit          exit");
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
        let mut registry = tau::ProviderRegistry::new();
        registry.register(Box::new(tau::providers::anthropic::Anthropic));
        let all_models = tau::providers::anthropic::models();
        let default_model = all_models.first().expect("at least one model").clone();
        tau::server::run(registry, default_model, all_models).await?;
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
    client.send(&tau::protocol::Request::Shutdown).await?;
    client.recv_streaming(|_| {}).await?;
    eprintln!("server stopped");
    Ok(())
}

async fn cmd_server_restart() -> tau::Result<()> {
    if tau::server::is_running() {
        let mut client = tau::client::Client::connect().await?;
        client.send(&tau::protocol::Request::Shutdown).await?;
        client.recv_streaming(|_| {}).await?;
        // Wait for socket to go away
        for _ in 0..50 {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            if !tau::server::is_running() {
                break;
            }
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

async fn cmd_sessions_list() -> tau::Result<()> {
    let mut client = tau::client::Client::connect_or_start().await?;
    client.send(&tau::protocol::Request::ListSessions).await?;

    client
        .recv_streaming(|resp| {
            if let tau::protocol::Response::Sessions { sessions } = resp {
                if sessions.is_empty() {
                    println!("no sessions");
                } else {
                    for s in sessions {
                        let stats = tau::protocol::format_stats(&s.stats);
                        println!("{}\t{}/{}\t{}", s.id, s.provider, s.model, stats);
                    }
                }
            }
        })
        .await?;
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
