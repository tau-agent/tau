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
        #[arg(long, default_value = "claude-sonnet-4-20250514")]
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
                // Direct file operation, no server needed
                let auth = tau::auth::AuthStorage::open_default();
                auth.remove(&provider)?;
                eprintln!("logged out from {}", provider);
            }
        },
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async fn cmd_login(provider: &str) -> tau::Result<()> {
    // Login runs directly in the CLI process (needs to open browser + wait for callback).
    // We don't go through the server because the callback server binds a port locally.
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

    // Create or reuse session
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

    if let Some(text) = message {
        // One-shot mode
        send_and_print(&mut client, &session_id, &text).await?;
    } else {
        // Interactive mode
        interactive_loop(&mut client, &session_id).await?;
    }

    Ok(())
}

async fn send_and_print(
    client: &mut tau::client::Client,
    session_id: &str,
    text: &str,
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
                tau::StreamEvent::Done { .. } => {
                    println!();
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

async fn interactive_loop(client: &mut tau::client::Client, session_id: &str) -> tau::Result<()> {
    use std::io::Write;

    loop {
        print!("tau> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/quit" || line == "/exit" {
            break;
        }

        send_and_print(client, session_id, line).await?;
    }

    Ok(())
}

async fn cmd_server_start(foreground: bool) -> tau::Result<()> {
    if tau::server::is_running() {
        eprintln!("server already running");
        return Ok(());
    }

    if foreground {
        let mut registry = tau::ProviderRegistry::new();
        registry.register(Box::new(tau::providers::anthropic::Anthropic));
        let default_model = tau::providers::anthropic::models()
            .into_iter()
            .next()
            .expect("at least one model");
        tau::server::run(registry, default_model).await?;
    } else {
        let exe = std::env::current_exe().map_err(|e| tau::Error::Io(e.to_string()))?;
        let child = std::process::Command::new(exe)
            .args(["server", "start", "--foreground"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| tau::Error::Io(format!("spawn: {}", e)))?;
        eprintln!("server started (pid {})", child.id());

        for _ in 0..50 {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            if tau::server::is_running() {
                eprintln!("server ready at {}", tau::server::socket_path().display());
                return Ok(());
            }
        }
        eprintln!("warning: server may not have started");
    }
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
                        println!(
                            "{}\t{}\t{}\t{} msgs",
                            s.id, s.provider, s.model, s.message_count
                        );
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
