//! End-to-end tests for `Request::ReloadConfig`.
//!
//! The reload handler reads `providers.toml` via `paths::config_dir()`,
//! which resolves to `$XDG_CONFIG_HOME/tau`. Env vars are process-global,
//! so the tests in this file serialize on a module-level mutex and each
//! stage sets a per-test tempdir as `XDG_CONFIG_HOME` while holding the
//! lock.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};

use tau_agent_lib::protocol::{Request, Response};

use common::TestServer;

/// Serialize env-var mutation across the tests in this file.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Guard that sets `XDG_CONFIG_HOME` for the duration of a test and
/// restores the previous value (including unset) on drop. We also clear
/// `HOME` so the fallback branch in `paths::config_dir` never picks up
/// the developer's real `~/.config/tau` while a test is running.
struct ConfigDirGuard {
    prev_xdg: Option<String>,
    prev_home: Option<String>,
}

impl ConfigDirGuard {
    fn set(path: &std::path::Path) -> Self {
        let guard = Self {
            prev_xdg: std::env::var("XDG_CONFIG_HOME").ok(),
            prev_home: std::env::var("HOME").ok(),
        };
        // SAFETY: serialized by `env_lock`; no other tests in this file
        // touch these env vars while we hold the lock. Other test files
        // run in their own process.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", path);
            std::env::remove_var("HOME");
        }
        guard
    }
}

impl Drop for ConfigDirGuard {
    fn drop(&mut self) {
        // SAFETY: serialized by `env_lock`.
        unsafe {
            match &self.prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match &self.prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

/// Send a `Request::ReloadConfig` and return the single `Response`.
fn send_reload(server: &TestServer) -> Response {
    let stream = UnixStream::connect(&server.sock_path).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut line = serde_json::to_string(&Request::ReloadConfig).unwrap();
    line.push('\n');
    w.write_all(line.as_bytes()).unwrap();
    w.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).unwrap();
    serde_json::from_str(&resp_line).unwrap()
}

/// List models, returning the `(provider, id)` pairs on the server.
fn list_models(server: &TestServer) -> Vec<(String, String)> {
    let stream = UnixStream::connect(&server.sock_path).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let resp = common::send_recv(&stream, &Request::ListModels);
    match resp {
        Response::Models { models } => models
            .into_iter()
            .map(|m| (m.provider, m.id))
            .collect::<Vec<_>>(),
        other => panic!("expected Models, got {:?}", other),
    }
}

/// `providers.toml` fragment that preserves the hard-coded mock provider
/// from `run_with_config` so the reload path doesn't lose it. The mock
/// provider itself has no models — `mock_model()` (which is what the
/// test server advertises as its default) does not round-trip through
/// `resolve_models`, it is added to `all_models` separately by the
/// `TestServerConfig`. After a reload it is therefore gone, and the
/// server picks a new default from whatever `resolve_models` produced.
const MOCK_PROVIDER_TOML: &str = r#"
[providers.mock]
api = "openai"
base_url = "http://mock"
api_key = "mock-key"
"#;

fn write_providers_toml(config_dir: &std::path::Path, extra: &str) {
    let tau_dir = config_dir.join("tau");
    std::fs::create_dir_all(&tau_dir).unwrap();
    let full = format!("{}{}", MOCK_PROVIDER_TOML, extra);
    std::fs::write(tau_dir.join("providers.toml"), full).unwrap();
}

#[test]
fn config_reload_picks_up_new_provider() {
    let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let xdg = tempfile::tempdir().unwrap();
    let _dir_guard = ConfigDirGuard::set(xdg.path());

    write_providers_toml(xdg.path(), ""); // start: just mock
    let server = TestServer::start(vec![]);

    // Baseline: the built-in anthropic/openai models are present, plus
    // the mock model from `TestServerConfig`. No provider named `acme`.
    let before = list_models(&server);
    assert!(
        !before.iter().any(|(p, _)| p == "acme"),
        "unexpected `acme` provider before reload: {:?}",
        before
    );

    // Add a new provider with one model and reload.
    write_providers_toml(
        xdg.path(),
        r#"
[providers.acme]
api = "openai"
base_url = "https://acme.test/v1"
api_key = "none"

[[providers.acme.models]]
id = "acme-smart"
context_window = 200000
max_tokens = 8192
"#,
    );

    let resp = send_reload(&server);
    assert!(matches!(resp, Response::Ok), "expected Ok, got {:?}", resp);

    let after = list_models(&server);
    assert!(
        after
            .iter()
            .any(|(p, id)| p == "acme" && id == "acme-smart"),
        "expected acme/acme-smart after reload, got {:?}",
        after
    );

    // A second reload adding a further provider should also take effect.
    write_providers_toml(
        xdg.path(),
        r#"
[providers.acme]
api = "openai"
base_url = "https://acme.test/v1"
api_key = "none"

[[providers.acme.models]]
id = "acme-smart"
context_window = 200000
max_tokens = 8192

[providers.beta]
api = "openai"
base_url = "https://beta.test/v1"
api_key = "none"

[[providers.beta.models]]
id = "beta-fast"
context_window = 100000
max_tokens = 4096
"#,
    );
    let resp = send_reload(&server);
    assert!(matches!(resp, Response::Ok), "expected Ok, got {:?}", resp);

    let after2 = list_models(&server);
    assert!(
        after2
            .iter()
            .any(|(p, id)| p == "acme" && id == "acme-smart"),
        "expected acme/acme-smart on second reload, got {:?}",
        after2
    );
    assert!(
        after2
            .iter()
            .any(|(p, id)| p == "beta" && id == "beta-fast"),
        "expected beta/beta-fast on second reload, got {:?}",
        after2
    );

    server.shutdown();
}

#[test]
fn config_reload_preserves_default_model_when_possible() {
    let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let xdg = tempfile::tempdir().unwrap();
    let _dir_guard = ConfigDirGuard::set(xdg.path());

    // Start with a providers.toml that already advertises a model
    // matching the test server's default `mock_model()` — this way
    // the reload path can preserve it.
    write_providers_toml(
        xdg.path(),
        r#"
[[providers.mock.models]]
id = "mock-model"
name = "Mock Model"
context_window = 100000
max_tokens = 4096
"#,
    );
    let server = TestServer::start(vec![]);

    let resp = send_reload(&server);
    assert!(matches!(resp, Response::Ok), "expected Ok, got {:?}", resp);

    // After reload, mock/mock-model must still be present so the
    // default-model preservation logic can find it.
    let after = list_models(&server);
    assert!(
        after
            .iter()
            .any(|(p, id)| p == "mock" && id == "mock-model"),
        "expected mock/mock-model to survive reload, got {:?}",
        after
    );

    // Indirect check: a CreateSession with no explicit model should
    // still yield a session whose model id is `mock-model` — the
    // preservation path picks the old default by (provider, id).
    let conn = server.connect();
    let sid = match common::send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            cwd: None,
            parent_id: None,
            system_prompt: None,
            child_budget: 0,
            project_name: None,
            tagline: None,
            auto_archive: false,
            notify_parent: true,
            sandbox_profile: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    let info = common::get_session_info(&server, &sid).expect("session info");
    assert_eq!(info.model, "mock-model", "default model identity lost");
    assert_eq!(info.provider, "mock", "default provider identity lost");

    server.shutdown();
}

#[test]
fn config_reload_rejects_broken_toml_without_corrupting_state() {
    let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let xdg = tempfile::tempdir().unwrap();
    let _dir_guard = ConfigDirGuard::set(xdg.path());

    write_providers_toml(xdg.path(), ""); // start: just mock, valid
    let server = TestServer::start(vec![]);
    let before = list_models(&server);

    // Write a syntactically bad providers.toml.
    let tau_dir = xdg.path().join("tau");
    std::fs::write(tau_dir.join("providers.toml"), "not valid = = toml =").unwrap();

    let resp = send_reload(&server);
    match resp {
        Response::Error { message } => {
            assert!(
                message.contains("reload config"),
                "expected reload-config error prefix, got {:?}",
                message
            );
        }
        other => panic!("expected Error for broken TOML, got {:?}", other),
    }

    // Pre-reload model list must be intact.
    let after = list_models(&server);
    assert_eq!(before, after, "model list changed after a failed reload");

    server.shutdown();
}
