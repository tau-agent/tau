//! Shared test helpers for e2e tests.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tau_agent_lib::protocol::{Request, Response, SessionInfo};
use tau_agent_lib::providers::mock::{MockProvider, MockResponse, mock_model};

/// Send a request and read one response line.
pub fn send_recv(stream: &UnixStream, req: &Request) -> Response {
    let mut stream = stream.try_clone().unwrap();
    let mut line = serde_json::to_string(req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).unwrap();
    serde_json::from_str(&resp_line).unwrap()
}

/// Read all response lines until a terminal one.
pub fn send_recv_all(stream: &UnixStream, req: &Request) -> Vec<Response> {
    let mut stream = stream.try_clone().unwrap();
    let mut line = serde_json::to_string(req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut responses = Vec::new();
    loop {
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).unwrap();
        if resp_line.trim().is_empty() {
            continue;
        }
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        let is_terminal = matches!(
            &resp,
            Response::SessionCreated { .. }
                | Response::SessionInfo { .. }
                | Response::SessionAncestors { .. }
                | Response::Sessions { .. }
                | Response::SessionDeleted
                | Response::SessionsCompleted { .. }
                | Response::AgentDone
                | Response::Cancelled
                | Response::MessageReply { .. }
                | Response::Ok
                | Response::OkWithNote { .. }
                | Response::Models { .. }
                | Response::Messages { .. }
                | Response::ToolExecuted { .. }
        );
        responses.push(resp);
        if is_terminal {
            break;
        }
    }
    responses
}

pub struct TestServer {
    pub sock_path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TestServer {
    /// Start a test server with mock provider in a background thread.
    pub fn start(mock_responses: Vec<MockResponse>) -> Self {
        // Keep shutdown snappy in tests; production defaults to 180s.
        // SAFETY: integration tests each run in their own process.
        unsafe {
            std::env::set_var("TAU_SHUTDOWN_DRAIN_SECS", "2");
        }
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("tau-test.sock");
        let db_path = dir.path().join("test.db");
        let sock_clone = sock_path.clone();

        let model = mock_model();
        let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
        registry.register(MockProvider::new(mock_responses));

        let config = tau_agent_lib::server::TestServerConfig {
            registry,
            models: vec![model],
            socket_path: sock_clone,
            db_path,
            tool_executor_factory: None,
            mock_tools: vec![],
            plugins_config: None,
            aliases: std::collections::HashMap::new(),
        };

        std::thread::spawn(move || {
            smol::block_on(async {
                if let Err(e) = tau_agent_lib::server::run_with_config(config).await {
                    eprintln!("test server error: {}", e);
                }
            });
        });

        // Wait for socket to appear
        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sock_path.exists(), "server socket did not appear");

        TestServer {
            sock_path,
            _dir: dir,
        }
    }

    /// Start a test server with custom config modifications.
    pub fn start_with_config<F>(mock_responses: Vec<MockResponse>, configure: F) -> Self
    where
        F: FnOnce(
            tau_agent_lib::server::TestServerConfig,
        ) -> tau_agent_lib::server::TestServerConfig,
    {
        // Keep shutdown snappy in tests; production defaults to 180s.
        // SAFETY: integration tests each run in their own process.
        unsafe {
            std::env::set_var("TAU_SHUTDOWN_DRAIN_SECS", "2");
        }
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("tau-test.sock");
        let db_path = dir.path().join("test.db");
        let sock_clone = sock_path.clone();

        let model = mock_model();
        let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
        registry.register(MockProvider::new(mock_responses));

        let base_config = tau_agent_lib::server::TestServerConfig {
            registry,
            models: vec![model],
            socket_path: sock_clone,
            db_path,
            tool_executor_factory: None,
            mock_tools: vec![],
            plugins_config: None,
            aliases: std::collections::HashMap::new(),
        };
        let config = configure(base_config);

        std::thread::spawn(move || {
            smol::block_on(async {
                if let Err(e) = tau_agent_lib::server::run_with_config(config).await {
                    eprintln!("test server error: {}", e);
                }
            });
        });

        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sock_path.exists(), "server socket did not appear");

        TestServer {
            sock_path,
            _dir: dir,
        }
    }

    /// Start a test server registered with ONLY the `log` provider and
    /// `log_model()`. Used by placeholder / no-agent-loop tests.
    pub fn start_log_only() -> Self {
        Self::start_with_config(vec![], |mut config| {
            use tau_agent_lib::providers::log::{LogProvider, log_model};
            let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
            registry.register(LogProvider);
            config.registry = registry;
            config.models = vec![log_model()];
            config
        })
    }

    /// Start a test server whose default model advertises a bogus provider
    /// slug, so `resolve_api_key` returns `None` and the agent runner
    /// early-returns `Err(NoApiKey)`. Used by the "no api key" error-path
    /// tests.
    pub fn start_without_api_key() -> Self {
        Self::start_with_config(vec![], |mut config| {
            // Register MockProvider under the "mock" api id so
            // `needs_api_key` returns true.  The model below uses a bogus
            // provider slug that is guaranteed not to match any auth
            // entry or env var — so the preflight `resolve_api_key`
            // returns None and we hit NoApiKey.
            let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
            registry.register(MockProvider::new(vec![]));
            config.registry = registry;
            let mut model = mock_model();
            model.id = "needs-key-model-583".into();
            model.provider = "bogus-provider-583-no-such-key".into();
            config.models = vec![model];
            config
        })
    }

    /// Start a test server registered with BOTH the mock provider (as the
    /// default model) and the log provider.  `mock_model()` is listed
    /// first so it becomes the server's default_model; `log_model()` is
    /// registered so explicit `model: "log"` requests resolve correctly.
    pub fn start_mock_plus_log() -> Self {
        Self::start_with_config(vec![], |mut config| {
            use tau_agent_lib::providers::log::{LogProvider, log_model};
            let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
            registry.register(MockProvider::new(vec![]));
            registry.register(LogProvider);
            config.registry = registry;
            config.models = vec![mock_model(), log_model()];
            config
        })
    }

    pub fn connect(&self) -> UnixStream {
        let conn = UnixStream::connect(&self.sock_path).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        conn
    }

    pub fn shutdown(&self) {
        let conn = self.connect();
        send_recv(&conn, &Request::Shutdown { restart: false });
    }
}

// ---------------------------------------------------------------------------
// Polling helpers for task-lifecycle assertions.
//
// These helpers underpin the e2e tests that assert auto-dispatch actually
// produced a worker session (would have caught #572/#577/#584/#587).
// ---------------------------------------------------------------------------

/// Issue a `GetSessionInfo` against the server and return the parsed
/// `SessionInfo`.  Returns `None` when the server reports an error
/// (e.g. session does not exist).
pub fn get_session_info(server: &TestServer, session_id: &str) -> Option<SessionInfo> {
    let conn = server.connect();
    match send_recv(
        &conn,
        &Request::GetSessionInfo {
            session_id: session_id.to_string(),
        },
    ) {
        Response::SessionInfo { info } => Some(info),
        Response::Error { .. } => None,
        other => panic!("expected SessionInfo for {}, got: {:?}", session_id, other),
    }
}

/// Assert a session exists on the server, optionally checking parent and
/// tagline-prefix invariants.  Returns the `SessionInfo` for further
/// assertions by the caller.
///
/// Panics with a descriptive message when any check fails — these
/// assertions are the whole point of tests built on top of this helper.
pub fn assert_session_exists(
    server: &TestServer,
    target_session_id: &str,
    expected_parent: Option<&str>,
    expected_tagline_prefix: Option<&str>,
) -> SessionInfo {
    let info = get_session_info(server, target_session_id).unwrap_or_else(|| {
        panic!(
            "expected session {} to exist on the server, but GetSessionInfo returned an error",
            target_session_id
        )
    });
    if let Some(parent) = expected_parent {
        let actual = info.parent_id.as_deref().unwrap_or("");
        assert_eq!(
            actual, parent,
            "session {} parent mismatch: expected {}, got {}",
            target_session_id, parent, actual
        );
    }
    if let Some(prefix) = expected_tagline_prefix {
        let tagline = info.tagline.as_deref().unwrap_or("");
        assert!(
            tagline.starts_with(prefix),
            "session {} tagline {:?} does not start with expected prefix {:?}",
            target_session_id,
            tagline,
            prefix
        );
    }
    info
}

/// Fetch all messages from a session via `GetMessages`.
pub fn get_session_messages(
    server: &TestServer,
    session_id: &str,
) -> Vec<tau_agent_lib::types::Message> {
    let conn = server.connect();
    match send_recv(
        &conn,
        &Request::GetMessages {
            session_id: session_id.to_string(),
        },
    ) {
        Response::Messages { messages } => messages,
        other => panic!("expected Messages for {}, got: {:?}", session_id, other),
    }
}

/// Poll the given closure every `interval` until it returns `Some(value)`
/// or `timeout` elapses.  On timeout, panics with `description`.
pub fn poll_until<T, F>(timeout: Duration, interval: Duration, description: &str, mut f: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() > deadline {
            panic!("poll_until timed out after {:?}: {}", timeout, description);
        }
        std::thread::sleep(interval);
    }
}

// ---------------------------------------------------------------------------
// CreateSessionBuilder — ergonomic helper for building `Request::CreateSession`
// in tests, collapsing ~12-line literals down to a fluent one-liner.
//
// Defaults mirror the dominant pattern observed across the e2e suite:
//   model / provider / system_prompt / cwd / parent_id / tagline /
//   project_name / sandbox_profile = None
//   child_budget = 0
//   auto_archive = false
//   notify_parent = true
// ---------------------------------------------------------------------------

pub struct CreateSessionBuilder<'a> {
    server: Option<&'a TestServer>,
    model: Option<String>,
    provider: Option<String>,
    system_prompt: Option<String>,
    cwd: Option<String>,
    parent_id: Option<String>,
    child_budget: u32,
    tagline: Option<String>,
    auto_archive: bool,
    notify_parent: bool,
    project_name: Option<String>,
    sandbox_profile: Option<String>,
}

impl<'a> CreateSessionBuilder<'a> {
    pub fn new(server: &'a TestServer) -> Self {
        Self {
            server: Some(server),
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
            sandbox_profile: None,
        }
    }

    /// Construct a builder without an attached `TestServer`. The caller
    /// is responsible for sending the resulting `Request` over their own
    /// connection — useful for tests that drive raw `UnixStream`s directly.
    pub fn standalone() -> Self {
        Self {
            server: None,
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
            sandbox_profile: None,
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn model_opt(mut self, model: Option<impl Into<String>>) -> Self {
        self.model = model.map(Into::into);
        self
    }

    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn provider_opt(mut self, provider: Option<impl Into<String>>) -> Self {
        self.provider = provider.map(Into::into);
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn system_prompt_opt(mut self, prompt: Option<impl Into<String>>) -> Self {
        self.system_prompt = prompt.map(Into::into);
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn cwd_opt(mut self, cwd: Option<impl Into<String>>) -> Self {
        self.cwd = cwd.map(Into::into);
        self
    }

    pub fn parent(mut self, parent_id: impl Into<String>) -> Self {
        self.parent_id = Some(parent_id.into());
        self
    }

    pub fn parent_opt(mut self, parent_id: Option<impl Into<String>>) -> Self {
        self.parent_id = parent_id.map(Into::into);
        self
    }

    pub fn child_budget(mut self, n: u32) -> Self {
        self.child_budget = n;
        self
    }

    pub fn tagline(mut self, tagline: impl Into<String>) -> Self {
        self.tagline = Some(tagline.into());
        self
    }

    pub fn tagline_opt(mut self, tagline: Option<impl Into<String>>) -> Self {
        self.tagline = tagline.map(Into::into);
        self
    }

    pub fn notify_parent(mut self, notify: bool) -> Self {
        self.notify_parent = notify;
        self
    }

    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.project_name = Some(project.into());
        self
    }

    pub fn project_opt(mut self, project: Option<impl Into<String>>) -> Self {
        self.project_name = project.map(Into::into);
        self
    }

    /// Build the `Request::CreateSession` value without sending it.
    /// Useful for unit-testing the builder itself.
    pub fn build(&self) -> Request {
        Request::CreateSession {
            model: self.model.clone(),
            provider: self.provider.clone(),
            system_prompt: self.system_prompt.clone(),
            cwd: self.cwd.clone(),
            parent_id: self.parent_id.clone(),
            child_budget: self.child_budget,
            tagline: self.tagline.clone(),
            auto_archive: self.auto_archive,
            notify_parent: self.notify_parent,
            project_name: self.project_name.clone(),
            sandbox_profile: self.sandbox_profile.clone(),
        }
    }

    /// Send the `CreateSession` request and return the new session id.
    /// Panics on any non-`SessionCreated` response.
    pub fn send(self) -> String {
        let req = self.build();
        let server = self.server.expect(
            "CreateSessionBuilder::send requires a TestServer; use build() for raw connections",
        );
        let conn = server.connect();
        match send_recv(&conn, &req) {
            Response::SessionCreated { session_id } => session_id,
            other => panic!("expected SessionCreated, got {:?}", other),
        }
    }

    /// Send the `CreateSession` request and return the raw response,
    /// for tests that want to assert an error was returned.
    pub fn send_raw(self) -> Response {
        let req = self.build();
        let server = self.server.expect(
            "CreateSessionBuilder::send_raw requires a TestServer; use build() for raw connections",
        );
        let conn = server.connect();
        send_recv(&conn, &req)
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    fn dummy_server() -> TestServer {
        let dir = tempfile::tempdir().expect("tempdir");
        TestServer {
            sock_path: dir.path().join("unused.sock"),
            _dir: dir,
        }
    }

    #[test]
    fn builder_build_defaults() {
        let server = dummy_server();
        let req = CreateSessionBuilder::new(&server).build();
        match req {
            Request::CreateSession {
                model,
                provider,
                system_prompt,
                cwd,
                parent_id,
                child_budget,
                tagline,
                auto_archive,
                notify_parent,
                project_name,
                sandbox_profile,
            } => {
                assert!(model.is_none());
                assert!(provider.is_none());
                assert!(system_prompt.is_none());
                assert!(cwd.is_none());
                assert!(parent_id.is_none());
                assert_eq!(child_budget, 0);
                assert!(tagline.is_none());
                assert!(!auto_archive);
                assert!(notify_parent);
                assert!(project_name.is_none());
                assert!(sandbox_profile.is_none());
            }
            other => panic!("expected CreateSession, got {:?}", other),
        }
    }

    #[test]
    fn builder_build_with_fields_set() {
        let server = dummy_server();
        let req = CreateSessionBuilder::new(&server)
            .model("m")
            .provider("p")
            .system_prompt("sp")
            .cwd("/tmp")
            .parent("s1")
            .child_budget(7)
            .tagline("tag")
            .notify_parent(false)
            .project("proj")
            .build();
        match req {
            Request::CreateSession {
                model,
                provider,
                system_prompt,
                cwd,
                parent_id,
                child_budget,
                tagline,
                notify_parent,
                project_name,
                auto_archive,
                sandbox_profile,
            } => {
                assert_eq!(model.as_deref(), Some("m"));
                assert_eq!(provider.as_deref(), Some("p"));
                assert_eq!(system_prompt.as_deref(), Some("sp"));
                assert_eq!(cwd.as_deref(), Some("/tmp"));
                assert_eq!(parent_id.as_deref(), Some("s1"));
                assert_eq!(child_budget, 7);
                assert_eq!(tagline.as_deref(), Some("tag"));
                assert!(!notify_parent);
                assert_eq!(project_name.as_deref(), Some("proj"));
                assert!(!auto_archive);
                assert!(sandbox_profile.is_none());
            }
            other => panic!("expected CreateSession, got {:?}", other),
        }
    }

    #[test]
    fn builder_opt_setters() {
        let server = dummy_server();
        let some_str: Option<&str> = Some("/home");
        let none_str: Option<&str> = None;
        let req_some = CreateSessionBuilder::new(&server)
            .cwd_opt(some_str)
            .model_opt(Some("m"))
            .project_opt(Some("p"))
            .build();
        let req_none = CreateSessionBuilder::new(&server)
            .cwd_opt(none_str)
            .model_opt(None::<String>)
            .project_opt(None::<&str>)
            .build();
        if let Request::CreateSession {
            cwd,
            model,
            project_name,
            ..
        } = req_some
        {
            assert_eq!(cwd.as_deref(), Some("/home"));
            assert_eq!(model.as_deref(), Some("m"));
            assert_eq!(project_name.as_deref(), Some("p"));
        } else {
            panic!("expected CreateSession");
        }
        if let Request::CreateSession {
            cwd,
            model,
            project_name,
            ..
        } = req_none
        {
            assert!(cwd.is_none());
            assert!(model.is_none());
            assert!(project_name.is_none());
        } else {
            panic!("expected CreateSession");
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Ok(mut conn) = UnixStream::connect(&self.sock_path) {
            let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
            let _ = conn.write_all(format!("{}\n", req).as_bytes());
            let _ = conn.flush();
        }
    }
}
