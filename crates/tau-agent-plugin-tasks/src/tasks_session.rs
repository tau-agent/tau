//! Centralised `Request::CreateSession` helper for task-related sessions.
//!
//! Every task-plugin code path that spawns a session (planner, refiner,
//! worker, reviewer, merge log session, interactive) used to open-code the
//! same ~20-line `Request::CreateSession { ... }` + response match. This
//! module consolidates that into [`create_task_session`] so callers only
//! specify the fields that actually vary (model, cwd, parent_id, role,
//! child_budget, sandbox_profile) and everything else (`provider: None`,
//! `system_prompt: None`, `auto_archive: false`, `notify_parent: false`,
//! `project_name` inherited from the task, `tagline` derived from role) is
//! filled in uniformly.
//!
//! The *placeholder* session (see `create_placeholder_session` in
//! `tasks.rs`) is intentionally NOT routed through this helper: it uses
//! [`crate::tasks_notify::task_placeholder_tagline`] instead of
//! [`crate::tasks_notify::task_session_tagline`] (the placeholder spans
//! every role and so has no role suffix), and its error path is
//! best-effort with DB-side logging that doesn't fit the simple
//! `Result<String>` signature. Folding it in would mean either adding a
//! custom-tagline escape hatch or bifurcating `task_session_tagline` —
//! both erode the helper's value for negligible savings. See task #604.

use std::io::{BufRead, Write};

use crate::tasks_db::Task;
use crate::tasks_notify::task_session_tagline;
use crate::tasks_scheduler::server_request;

/// Fields that vary per task-session call site.
///
/// Every other `CreateSession` field is filled in by [`create_task_session`]
/// with the values that are identical across every task-plugin call site.
pub(crate) struct TaskSessionSpec<'a> {
    pub task: &'a Task,
    /// Session role — one of `"worker"`, `"planning"`, `"review"`,
    /// `"refining"`, `"merge"`, `"interactive"`. Used to build the
    /// tagline via [`task_session_tagline`].
    pub role: &'static str,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub parent_id: Option<String>,
    /// Default: 16 for LLM sessions, 0 for log-only sessions (merge).
    pub child_budget: u32,
    pub sandbox_profile: Option<String>,
}

/// Create a session for a task, returning the new session id.
///
/// Fills in every field that's identical across task-plugin call sites
/// (`provider: None`, `system_prompt: None`, `auto_archive: false`,
/// `notify_parent: false`, `project_name: Some(task.project_name.clone())`,
/// `tagline: Some(task_session_tagline(task, role))`) and forwards the
/// rest from `spec`.
///
/// Errors propagate as [`tau_agent_plugin::Error::Io`] with a message
/// that identifies the task id and role for easier log triage.
pub(crate) fn create_task_session(
    spec: TaskSessionSpec<'_>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<String> {
    let req = tau_agent_plugin::Request::CreateSession {
        model: spec.model,
        provider: None,
        system_prompt: None,
        cwd: spec.cwd,
        parent_id: spec.parent_id,
        child_budget: spec.child_budget,
        tagline: Some(task_session_tagline(spec.task, spec.role)),
        auto_archive: false,
        notify_parent: false,
        project_name: Some(spec.task.project_name.clone()),
        sandbox_profile: spec.sandbox_profile,
    };
    match server_request(writer, reader, req)? {
        tau_agent_plugin::Response::SessionCreated { session_id } => Ok(session_id),
        tau_agent_plugin::Response::Error { message } => Err(tau_agent_plugin::Error::Io(format!(
            "create {} session for task {}: {}",
            spec.role, spec.task.id, message
        ))),
        other => Err(tau_agent_plugin::Error::Io(format!(
            "unexpected response creating {} session for task {}: {:?}",
            spec.role, spec.task.id, other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks_state::TaskState;
    use std::io::BufReader;
    use tau_agent_plugin::{PluginMessage, PluginRequest, Request, Response};

    fn fake_task(id: i64, title: &str) -> Task {
        Task {
            id,
            project_name: "demo".to_string(),
            title: title.to_string(),
            state: TaskState::Ready,
            priority: 0,
            parent_id: None,
            tags: None,
            affected_files: None,
            branch: None,
            merge_target: None,
            worktree_path: None,
            session_id: None,
            skip_review: false,
            require_approval: false,
            sandbox_profile: None,
            held: false,
            placeholder_session_id: None,
            auto_downgraded_from_ready: false,
            filed_by_project: None,
            filed_by_session_id: None,
            no_merge: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Build a `ServerResponse` JSON line that `server_request` will accept.
    ///
    /// `server_request` matches on `request_id`, so we sniff the writer output
    /// (after the call) for the actual id — but since we need the reader to
    /// contain the response *before* the call, we pre-build a response with a
    /// wildcard id that the protocol layer accepts: any `ServerResponse` line
    /// with a matching prefix is accepted if it's the only candidate on the
    /// wire. Actually the matcher is strict-equality, so we need a different
    /// approach: run the call on a thread and feed the response once we've
    /// observed the emitted request. To keep tests synchronous we drive the
    /// protocol ourselves via a pipe that mirrors the id back.
    ///
    /// The helper here constructs a reader that, on each `read_line`, first
    /// inspects what the writer has emitted so far, extracts the last
    /// `ServerRequest`'s `request_id`, and then yields a pre-canned
    /// `ServerResponse` line with that id. This lets us unit-test
    /// `create_task_session` without spinning threads.
    struct RespondingReader {
        writer: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        response: Response,
        buf: Vec<u8>,
    }

    impl std::io::Read for RespondingReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.buf.is_empty() {
                // Pull the most recent ServerRequest's request_id out of the
                // writer buffer to mirror it back in the response.
                let written = self.writer.lock().expect("writer mutex poisoned").clone();
                let text = String::from_utf8_lossy(&written);
                let mut request_id: Option<String> = None;
                for line in text.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(PluginMessage::ServerRequest {
                        request_id: rid, ..
                    }) = serde_json::from_str::<PluginMessage>(line)
                    {
                        request_id = Some(rid);
                    }
                }
                let rid = request_id
                    .expect("RespondingReader: expected a ServerRequest to be emitted before read");
                let line = serde_json::to_string(&PluginRequest::ServerResponse {
                    request_id: rid,
                    response: self.response.clone(),
                })
                .expect("serialize ServerResponse");
                self.buf = line.into_bytes();
                self.buf.push(b'\n');
            }
            let n = std::cmp::min(out.len(), self.buf.len());
            out[..n].copy_from_slice(&self.buf[..n]);
            self.buf.drain(..n);
            Ok(n)
        }
    }

    /// A writer that appends to a shared buffer so the `RespondingReader`
    /// can observe it.
    struct SharedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("writer mutex poisoned")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        spec_task: Task,
        role: &'static str,
        model: Option<String>,
        cwd: Option<String>,
        parent_id: Option<String>,
        child_budget: u32,
        sandbox_profile: Option<String>,
        response: Response,
    ) -> (tau_agent_plugin::Result<String>, Vec<u8>) {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let mut writer = SharedWriter(shared.clone());
        let reader = RespondingReader {
            writer: shared.clone(),
            response,
            buf: Vec::new(),
        };
        let mut reader = BufReader::new(reader);
        let result = create_task_session(
            TaskSessionSpec {
                task: &spec_task,
                role,
                model,
                cwd,
                parent_id,
                child_budget,
                sandbox_profile,
            },
            &mut writer,
            &mut reader,
        );
        let emitted = shared.lock().expect("writer mutex poisoned").clone();
        (result, emitted)
    }

    fn extract_create_session(emitted: &[u8]) -> Request {
        let text = String::from_utf8_lossy(emitted);
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(PluginMessage::ServerRequest { request: req, .. }) =
                serde_json::from_str::<PluginMessage>(line)
            {
                if matches!(req, Request::CreateSession { .. }) {
                    return req;
                }
            }
        }
        panic!("no CreateSession request found in writer output: {}", text);
    }

    #[test]
    fn happy_path_returns_session_id_and_fills_invariants() {
        let (result, emitted) = run(
            fake_task(1, "Test title"),
            "worker",
            Some("haiku".to_string()),
            Some("/tmp/worktree".to_string()),
            Some("s-parent".to_string()),
            16,
            Some("tight".to_string()),
            Response::SessionCreated {
                session_id: "s999".to_string(),
            },
        );
        assert_eq!(result.expect("create_task_session should succeed"), "s999");

        let req = extract_create_session(&emitted);
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
                // Forwarded fields.
                assert_eq!(model.as_deref(), Some("haiku"));
                assert_eq!(cwd.as_deref(), Some("/tmp/worktree"));
                assert_eq!(parent_id.as_deref(), Some("s-parent"));
                assert_eq!(child_budget, 16);
                assert_eq!(sandbox_profile.as_deref(), Some("tight"));

                // Invariants the helper centralises.
                assert!(provider.is_none(), "provider must always be None");
                assert!(system_prompt.is_none(), "system_prompt must always be None");
                assert!(!auto_archive, "auto_archive must always be false");
                assert!(!notify_parent, "notify_parent must always be false");
                assert_eq!(
                    project_name.as_deref(),
                    Some("demo"),
                    "project_name must come from the task"
                );
                assert_eq!(
                    tagline.as_deref(),
                    Some("[task 1] worker: Test title"),
                    "tagline must be derived from task_session_tagline(task, role)"
                );
            }
            other => panic!("expected CreateSession, got {:?}", other),
        }
    }

    #[test]
    fn error_response_is_propagated() {
        let (result, _) = run(
            fake_task(42, "Broken"),
            "worker",
            None,
            None,
            None,
            16,
            None,
            Response::Error {
                message: "boom".to_string(),
            },
        );
        let err = result.expect_err("create_task_session should fail on Error response");
        let msg = format!("{}", err);
        assert!(msg.contains("task 42"), "error should name task id: {msg}");
        assert!(msg.contains("worker"), "error should name role: {msg}");
        assert!(
            msg.contains("boom"),
            "error should include server message: {msg}"
        );
    }

    #[test]
    fn unexpected_response_is_rejected() {
        let (result, _) = run(
            fake_task(7, "Oddball"),
            "planning",
            None,
            None,
            None,
            16,
            None,
            Response::Ok,
        );
        let err = result.expect_err("create_task_session should fail on unexpected response");
        let msg = format!("{}", err);
        assert!(
            msg.contains("unexpected response"),
            "error should mention unexpected response: {msg}"
        );
        assert!(msg.contains("task 7"), "error should name task id: {msg}");
        assert!(msg.contains("planning"), "error should name role: {msg}");
    }

    #[test]
    fn merge_role_preserves_zero_budget_and_log_model() {
        let (result, emitted) = run(
            fake_task(5, "Merge me"),
            "merge",
            Some("log".to_string()),
            Some("/tmp/wt".to_string()),
            Some("s-placeholder".to_string()),
            0,
            None,
            Response::SessionCreated {
                session_id: "s-merge".to_string(),
            },
        );
        assert_eq!(result.expect("merge session should be created"), "s-merge");

        let req = extract_create_session(&emitted);
        match req {
            Request::CreateSession {
                model,
                child_budget,
                tagline,
                ..
            } => {
                assert_eq!(
                    model.as_deref(),
                    Some("log"),
                    "merge session must retain log model"
                );
                assert_eq!(
                    child_budget, 0,
                    "merge session must retain child_budget: 0 (no silent defaulting to 16)"
                );
                assert_eq!(
                    tagline.as_deref(),
                    Some("[task 5] merge: Merge me"),
                    "merge session must go through task_session_tagline, not task_placeholder_tagline"
                );
            }
            other => panic!("expected CreateSession, got {:?}", other),
        }
    }

    #[test]
    fn interactive_role_uses_session_tagline() {
        let (result, emitted) = run(
            fake_task(9, "Chat with me"),
            "interactive",
            None,
            None,
            None,
            16,
            None,
            Response::SessionCreated {
                session_id: "s-int".to_string(),
            },
        );
        assert_eq!(
            result.expect("interactive session should be created"),
            "s-int"
        );

        let req = extract_create_session(&emitted);
        match req {
            Request::CreateSession { tagline, .. } => {
                assert_eq!(
                    tagline.as_deref(),
                    Some("[task 9] interactive: Chat with me"),
                    "interactive session must go through task_session_tagline, not \
                     task_placeholder_tagline"
                );
            }
            other => panic!("expected CreateSession, got {:?}", other),
        }
    }

    /// Regression guard: the whole point of this module is to be the *only*
    /// construction site for `Request::CreateSession` in the tasks plugin.
    ///
    /// We scan every `src/*.rs` file *except* this one and assert exactly
    /// **one** bare `Request::CreateSession { ... }` construction site
    /// remains, in `tasks.rs` (the documented placeholder exception at the
    /// top of `create_placeholder_session`; it uses
    /// `task_placeholder_tagline` and has bespoke DB-logging error
    /// handling that doesn't fit the helper).
    ///
    /// If a future refactor folds the placeholder in, the count drops to 0
    /// and this test will need updating (acceptable one-line maintenance
    /// cost). If a new call site sneaks in and bypasses the helper, this
    /// test fails and the reviewer is nudged to route it through
    /// [`create_task_session`].
    #[test]
    fn no_bare_create_session_literals_beyond_helper_and_placeholder() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let src_dir = std::path::Path::new(manifest_dir).join("src");
        let mut hits: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&src_dir).expect("read tau-agent-plugin-tasks/src directory")
        {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string();
            // Skip this helper's own file — it legitimately contains the
            // one sanctioned construction site plus match-arm patterns and
            // string-literal copies of the marker inside test scaffolding.
            if file_name == "tasks_session.rs" {
                continue;
            }
            let contents = std::fs::read_to_string(&path).expect("read src file");
            for (lineno, line) in contents.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with("///") {
                    continue;
                }
                if !line.contains("Request::CreateSession {") {
                    continue;
                }
                // Skip `match` arms / destructuring patterns — they are
                // deconstruction, not construction.
                if line.contains("Request::CreateSession { .. }") {
                    continue;
                }
                hits.push(format!("{}:{}: {}", file_name, lineno + 1, line.trim()));
            }
        }
        hits.sort();

        assert_eq!(
            hits.len(),
            1,
            "expected exactly 1 bare `Request::CreateSession {{` construction site \
             outside this helper (the documented placeholder exception in \
             tasks.rs); found {}:\n{}",
            hits.len(),
            hits.join("\n")
        );
        assert!(
            hits[0].starts_with("tasks.rs:"),
            "the sole remaining bare construction site must be the placeholder in \
             tasks.rs; found: {}",
            hits[0]
        );
    }
}
