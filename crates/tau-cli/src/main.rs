mod completer;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::ArgValueCandidates;

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
        #[arg(short, long, add = ArgValueCandidates::new(completer::session_completer))]
        session: Option<String>,
        /// Model name
        #[arg(long, default_value = "claude-sonnet-4-6",
              add = ArgValueCandidates::new(completer::model_completer))]
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
    /// Manage providers
    Providers {
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// Manage models
    Models {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Manage authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum ProviderAction {
    /// List configured providers
    List,
    /// Add a provider
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
    Remove {
        /// Provider name
        name: String,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// List all available models
    List,
    /// Add a model to a provider
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
        #[arg(add = ArgValueCandidates::new(completer::session_completer))]
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
        } => {
            cmd_chat(message, session, &model).await?;
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
) -> tau::Result<()> {
    let mut client = tau::client::Client::connect_or_start().await?;

    // Parse "provider/model" syntax
    let (provider, model_id) = if let Some(idx) = model.find('/') {
        (Some(model[..idx].to_string()), model[idx + 1..].to_string())
    } else {
        (None, model.to_string())
    };

    let session_id = if let Some(id) = session_id {
        id
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
                    _ => {}
                }
            }
            tau::protocol::Response::AgentDone => {
                totals.display();
            }
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

fn pct(b: Option<&tau::auth::UsageBucket>) -> String {
    match b.and_then(|b| b.utilization) {
        Some(u) => format!("{:.0}%", u),
        None => "?".into(),
    }
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
    session_id: &str,
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
