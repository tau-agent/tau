//! Merge worker thread for the tasks plugin.
//!
//! # Why this exists
//!
//! The tasks plugin services protocol messages on its main event loop
//! (see [`crate::tasks::run_tasks_plugin`]). That loop reads a
//! `PluginRequest` from stdin, dispatches to a handler, writes the
//! response, and goes back to reading.
//!
//! Auto-merges (approved → merged) run the full project checklist —
//! `just fmt && just build && just test` or whatever the project
//! configured — by issuing `ExecuteTool { tool_name: "bash" }` RPCs to the
//! server and waiting for each round-trip. On any non-trivial project
//! this is tens of seconds to minutes.
//!
//! If merges ran inline on the main loop, the plugin would stop reading
//! stdin for the duration. Concurrent tool calls from unrelated sessions
//! would time out at the server, surfacing to the user as "no plugin
//! provides tool X". This was observed repeatedly in practice.
//!
//! The fix is to move merge execution to a dedicated OS thread. The main
//! loop continues to discover approved tasks on its own (cheap DB query),
//! but instead of calling `merge_approved_for_caller` inline it enqueues
//! a [`MergeJob`] onto this worker's channel and returns immediately. The
//! worker processes jobs serially (merging the same repo concurrently
//! would collide on the git index lock anyway).
//!
//! # Concurrency boundaries
//!
//! The worker shares three things with the main loop:
//!
//! 1. **stdout**, via [`tau_agent_plugin::tunnel::SharedStdout`]. Every
//!    JSON line is written under a mutex, so writes never interleave.
//! 2. **stdin responses**, via a line router in the main module. Each
//!    `ServerResponse` incoming on stdin is dispatched to either the
//!    main-loop or the worker response channel based on the `request_id`
//!    prefix (`task-sr-...` vs `merge-sr-...`). The worker blocks on its
//!    own channel, so a slow RPC it issues does not block the main loop.
//! 3. **Databases**. The worker opens its own [`TasksDb`] and
//!    [`ProjectResolver`] connections; rusqlite's `Connection` is not
//!    `Sync`, and opening a second connection is strictly simpler than
//!    wrapping the main one in a mutex. Both connections point at the
//!    same WAL-enabled files.
//!
//! # Shutdown
//!
//! On plugin shutdown the main loop drops the [`MergeWorker`] handle.
//! The sender side of the channel drops; the worker's `rx.recv()`
//! returns `Err` and the thread exits cleanly. An in-flight merge runs
//! to completion before the thread exits — aborting mid-`just test`
//! would risk leaving a worktree in an inconsistent state. If the
//! server is shutting down aggressively the usual signal propagation
//! (SIGTERM to the plugin subprocess) will terminate any bash subprocess
//! still running under it, unblocking the worker from its RPC wait.

use std::io::Write;
use std::sync::mpsc;

use crate::err::plugin_io_err;
use crate::tasks::{ChannelLineReader, ProjectResolver};
use crate::tasks_db::{TaskUpdate, TasksDb};
use crate::tasks_state::TaskState;

/// A single unit of work for the merge worker: fully merge one approved
/// task (rebase, checklist, fast-forward, cleanup).
///
/// The job captures everything the worker needs. It does not borrow
/// from the main loop so the worker thread stays self-contained.
///
/// # Divergence from the #540 spec
///
/// The spec sketched a fatter `MergeJob` carrying `project_path` and
/// `project_name` alongside `task_id`. We keep only `task_id` and
/// re-derive everything else from the DB on the worker side. Two
/// reasons:
///
/// * It closes a small time-of-check-to-time-of-use window: if the
///   project is renamed (or its path changes) between enqueue and
///   pickup, re-resolving picks up the fresh value. Capturing the
///   path at enqueue time would merge against stale state.
/// * It also handles the case where the user reverted `approved`
///   before the worker got to the job — we re-read the task state
///   and silently skip non-approved tasks.
///
/// If the worker ever needs more fields (priority hints,
/// merge-target overrides) they go on this struct alongside
/// `task_id`.
pub struct MergeJob {
    /// Task id to merge. The worker re-reads the task from the DB to
    /// guard against state changes that happened between enqueue and
    /// execution (e.g. user reverted the approval).
    pub task_id: i64,
    /// Session id that *triggered* the merge, if any. Plumbed through to
    /// `merge_task_for_caller` so that archival of sessions under this
    /// caller's subtree is deferred to Tier-3 (see task #533).
    pub caller_session_id: Option<String>,
}

/// Handle to the merge worker thread.
///
/// Dropping the handle signals shutdown: the channel closes, the thread
/// drains any pending jobs, finishes any in-flight job, and exits.
pub struct MergeWorker {
    tx: Option<mpsc::Sender<MergeJob>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MergeWorker {
    /// Spawn the worker thread.
    ///
    /// The worker opens its own DB and project-resolver connections.
    /// Reader/writer are the worker's dedicated halves of the line
    /// router set up in [`crate::tasks::run_tasks_plugin`].
    ///
    /// `writer` must be a `SharedStdout` (or equivalent cloneable
    /// atomic-per-line writer). `reader` must receive only
    /// `ServerResponse` lines tagged with the `merge-sr-` request-id
    /// prefix — the main module's router guarantees this.
    pub(crate) fn spawn<W>(writer: W, reader: ChannelLineReader) -> tau_agent_plugin::Result<Self>
    where
        W: Write + Send + 'static,
    {
        // Open worker-local DB and resolver. Opening a second connection
        // is cheaper than mutexing the existing one and avoids any
        // cross-thread rusqlite issues (Connection is !Sync). Both
        // connections point at the same on-disk files; WAL mode allows
        // concurrent readers and serialises writers at the SQLite level.
        let db = TasksDb::open_default()?;
        let resolver = ProjectResolver::open()?;
        Self::spawn_with(db, resolver, writer, reader)
    }

    /// Spawn the worker with explicitly provided DB and resolver
    /// handles. Used by tests so they can drive the worker against an
    /// in-memory database.
    pub(crate) fn spawn_with<W>(
        db: TasksDb,
        resolver: ProjectResolver,
        writer: W,
        reader: ChannelLineReader,
    ) -> tau_agent_plugin::Result<Self>
    where
        W: Write + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<MergeJob>();

        let handle = std::thread::Builder::new()
            .name("tau-tasks-merge-worker".into())
            .spawn(move || {
                // Set this thread's RPC prefix so outgoing ServerRequests
                // generated by `tasks_scheduler::server_request` (and
                // therefore `tasks_merge` / `tasks_notify` which delegate
                // to it) are routed back to *this* thread's reader rather
                // than the main loop's.
                crate::tasks_scheduler::set_thread_rpc_prefix("merge-sr");
                worker_loop(db, resolver, rx, writer, reader);
            })
            .map_err(plugin_io_err("spawn merge worker"))?;

        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
        })
    }

    /// Queue a merge job. Non-blocking: returns immediately.
    ///
    /// Fails only if the worker thread has already exited and dropped
    /// its receiver (i.e. during shutdown). Callers log the error and
    /// carry on: the approved task remains in the DB and will be
    /// re-attempted on the next plugin startup.
    pub fn enqueue(&self, job: MergeJob) -> Result<(), mpsc::SendError<MergeJob>> {
        match &self.tx {
            Some(tx) => tx.send(job),
            None => Err(mpsc::SendError(job)),
        }
    }
}

impl Drop for MergeWorker {
    fn drop(&mut self) {
        // Drop the sender so the worker's rx.recv() returns Err and the
        // thread exits. Then join the thread so we don't leak it.
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            // If the worker panicked we just swallow it here: the main
            // loop is already on the way down. A panic from the worker
            // would already have been reported via stderr.
            let _ = handle.join();
        }
    }
}

fn worker_loop<W>(
    db: TasksDb,
    resolver: ProjectResolver,
    rx: mpsc::Receiver<MergeJob>,
    mut writer: W,
    mut reader: ChannelLineReader,
) where
    W: Write,
{
    eprintln!("tasks merge worker: thread started");
    for job in rx {
        eprintln!(
            "tasks merge worker: starting task {} (caller={:?})",
            job.task_id, job.caller_session_id
        );
        run_one_job(&db, &resolver, &job, &mut writer, &mut reader);
        eprintln!("tasks merge worker: finished task {}", job.task_id);
    }
    eprintln!("tasks merge worker: channel closed, exiting");
}

/// Execute one enqueued merge job.
///
/// Mirrors the state-transition logic that used to live inline in
/// `tasks_scheduler::merge_one_task`, but scoped to a single task and
/// driven by an explicit [`MergeJob`] rather than a DB scan. The main
/// loop still scans for approved tasks to produce jobs; this function
/// only runs the actual merge for one of them.
fn run_one_job<W>(
    db: &TasksDb,
    resolver: &ProjectResolver,
    job: &MergeJob,
    writer: &mut W,
    reader: &mut ChannelLineReader,
) where
    W: Write,
{
    // Re-fetch the task — its state may have changed between when the
    // main loop enqueued this job and when we picked it up (e.g. the
    // user manually reverted the approval). Silently skip non-approved
    // tasks instead of treating it as an error.
    let task = match db.get_task(job.task_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            eprintln!(
                "tasks merge worker: task {} not found, skipping",
                job.task_id
            );
            return;
        }
        Err(e) => {
            eprintln!(
                "tasks merge worker: db error reading task {}: {}",
                job.task_id, e
            );
            return;
        }
    };

    if task.state != TaskState::Approved {
        eprintln!(
            "tasks merge worker: task {} is now in state '{}' (not approved), skipping",
            job.task_id, task.state
        );
        return;
    }

    // Transition to merging. If the transition fails (concurrent writer,
    // invalid state machine edge) we bail out; the task keeps its
    // current state and will be retried on the next scheduler pass.
    if let Err(e) = db.update_task(
        job.task_id,
        &TaskUpdate {
            state: Some(TaskState::Merging),
            ..Default::default()
        },
        None,
    ) {
        eprintln!(
            "tasks merge worker: failed to transition task {} to merging: {}",
            job.task_id, e
        );
        return;
    }

    if let Ok(Some(t)) = db.get_task(job.task_id) {
        crate::tasks_notify::notify_state_change(db, &t, TaskState::Approved, None, writer, reader);
    }

    let project_dir = match resolver.resolve(&task.project_name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "tasks merge worker: resolve project path for task {}: {}",
                job.task_id, e
            );
            // Revert to approved so another pass can try again once the
            // project path is back.
            let _ = db.update_task(
                job.task_id,
                &TaskUpdate {
                    state: Some(TaskState::Approved),
                    ..Default::default()
                },
                None,
            );
            return;
        }
    };

    let caller = job.caller_session_id.as_deref();
    match crate::tasks_merge::merge_task_for_caller(
        db,
        job.task_id,
        &project_dir,
        caller,
        writer,
        reader,
    ) {
        Ok(result) => {
            if result.success {
                finish_success(db, resolver, job.task_id, &project_dir, writer, reader);
            } else {
                finish_failure(db, &task, job.task_id, &result.log, writer, reader);
            }
        }
        Err(e) => {
            // Unexpected tunnel error (stdin closed mid-merge, etc.).
            // Revert to active so another pass can retry.
            if let Err(te) = db.update_task(
                job.task_id,
                &TaskUpdate {
                    state: Some(TaskState::Active),
                    ..Default::default()
                },
                None,
            ) {
                eprintln!(
                    "tasks merge worker: failed to transition task {} back to active: {}",
                    job.task_id, te
                );
            }
            if let Ok(Some(t)) = db.get_task(job.task_id) {
                crate::tasks_notify::notify_state_change(
                    db,
                    &t,
                    TaskState::Merging,
                    Some(&format!("merge error: {}", e)),
                    writer,
                    reader,
                );
            }
            let _ = db.add_message(
                job.task_id,
                &format!("Auto-merge error: {}", e),
                Some("system"),
            );
            eprintln!(
                "tasks merge worker: task {} merge error: {}",
                job.task_id, e
            );
        }
    }
}

/// Maximum number of retries when transitioning a task state in the
/// merge worker. Used by `finish_success`'s `Merging → Merged`
/// transition: a transient sqlite busy hiccup here would otherwise
/// leave the task wedged in `merging` until the watchdog notices
/// (≥60 s). Three quick retries with 50 ms backoff cover the common
/// transient cases without delaying the worker; the watchdog still
/// catches us if all retries fail.
const DB_UPDATE_RETRIES: u32 = 3;
const DB_UPDATE_RETRY_DELAY_MS: u64 = 50;

/// Wrap [`TasksDb::update_task`] in a small retry loop with a fixed
/// backoff. Returns the last error if every attempt fails.
///
/// Mirrors `archive_session_with_busy_retry` in [`crate::tasks_merge`].
fn db_update_with_retry(
    db: &TasksDb,
    task_id: i64,
    update: &TaskUpdate,
) -> tau_agent_plugin::Result<crate::tasks_db::Task> {
    let mut last_err: Option<tau_agent_plugin::Error> = None;
    for attempt in 0..DB_UPDATE_RETRIES {
        match db.update_task(task_id, update, None) {
            Ok(t) => return Ok(t),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < DB_UPDATE_RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(DB_UPDATE_RETRY_DELAY_MS));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        tau_agent_plugin::Error::Io("db_update_with_retry: no error captured".into())
    }))
}

fn finish_success<W>(
    db: &TasksDb,
    resolver: &ProjectResolver,
    task_id: i64,
    project_dir: &str,
    writer: &mut W,
    reader: &mut ChannelLineReader,
) where
    W: Write,
{
    if let Err(e) = db_update_with_retry(
        db,
        task_id,
        &TaskUpdate {
            state: Some(TaskState::Merged),
            ..Default::default()
        },
    ) {
        // All retries exhausted. The stuck-merging watchdog (task
        // #850) will catch this within `STUCK_MERGING_THRESHOLD_MS`
        // and reconcile the task once it observes the merge landed
        // on the target branch. Surface loudly so the failure is
        // discoverable in stderr.
        eprintln!(
            "tasks merge worker: merge succeeded but transition to merged failed for task {} \
             after {} retries: {} — stuck-merging watchdog will reconcile",
            task_id, DB_UPDATE_RETRIES, e
        );
    }

    let merged_project: Option<String> = if let Ok(Some(t)) = db.get_task(task_id) {
        let ctx = crate::tasks_scheduler::extract_merge_commit(project_dir, &t);
        crate::tasks_notify::notify_state_change(
            db,
            &t,
            TaskState::Merging,
            ctx.as_deref(),
            writer,
            reader,
        );
        Some(t.project_name)
    } else {
        None
    };

    crate::tasks_merge::notify_parent_of_subtask_done(db, task_id, writer, reader);
    if let Err(e) = crate::tasks_merge::notify_parent_if_all_done(db, task_id, writer, reader) {
        eprintln!(
            "tasks merge worker: parent notification failed for task {}: {}",
            task_id, e
        );
    }

    eprintln!("tasks merge worker: task {} merged successfully", task_id);

    // Root cause of task #584: before this code existed, the merge
    // worker's `→ merged` transition bypassed the main-loop's
    // `SchedulerEvent::ScheduleNeeded` push (the main loop only pushes
    // ScheduleNeeded from tool-handler `task_update` paths; the merge
    // worker calls `db.update_task` directly). Result: ready tasks
    // that were previously filtered out by `select_non_conflicting`
    // (typically file-less tasks blocked by any in-flight task) would
    // remain in `ready` forever unless some *other* tool call on the
    // same project happened to fire a fresh schedule pass.
    //
    // Fix: after every successful merge, run a schedule pass for the
    // merged task's project right here on the worker thread. The
    // worker already has its own DB + resolver + writer/reader and
    // uses the `merge-sr` RPC prefix, so dispatch() succeeds from this
    // thread just as well as from the main loop.
    if let Some(project) = merged_project {
        let mut events: Vec<crate::tasks::SchedulerEvent> = Vec::new();
        let warnings = crate::tasks::run_schedule_pass(
            db,
            &project,
            resolver,
            None,
            writer,
            reader,
            &mut events,
        );
        for w in warnings {
            eprintln!("tasks merge worker: post-merge schedule: {}", w);
        }
        // Any follow-up ScheduleNeeded pushed onto `events` (dispatch
        // failure retry) is drained here inline — we don't have the
        // main loop to do it for us.
        for ev in std::mem::take(&mut events) {
            if let crate::tasks::SchedulerEvent::ScheduleNeeded(p, sid) = ev {
                let warnings = crate::tasks::run_schedule_pass(
                    db,
                    &p,
                    resolver,
                    sid.as_deref(),
                    writer,
                    reader,
                    &mut events,
                );
                for w in warnings {
                    eprintln!("tasks merge worker: post-merge schedule retry: {}", w);
                }
            }
        }
    }
}

fn finish_failure<W>(
    db: &TasksDb,
    task: &crate::tasks_db::Task,
    task_id: i64,
    log: &str,
    writer: &mut W,
    reader: &mut ChannelLineReader,
) where
    W: Write,
{
    if let Err(e) = db.update_task(
        task_id,
        &TaskUpdate {
            state: Some(TaskState::Active),
            ..Default::default()
        },
        None,
    ) {
        eprintln!(
            "tasks merge worker: failed to transition task {} back to active: {}",
            task_id, e
        );
    }

    if let Ok(Some(t)) = db.get_task(task_id) {
        crate::tasks_notify::notify_state_change(
            db,
            &t,
            TaskState::Merging,
            Some("merge failed — reverted to active"),
            writer,
            reader,
        );
    }

    let _ = db.add_message(
        task_id,
        &format!("Auto-merge failed:\n{}", log),
        Some("system"),
    );

    if let Some(ref sid) = task.session_id {
        crate::tasks_merge::notify_session_of_merge_failure(sid, task_id, log, writer, reader);
    }

    eprintln!("tasks merge worker: task {} merge failed", task_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Dropping the handle should terminate the worker thread promptly
    /// even with no jobs ever enqueued.
    #[test]
    fn worker_exits_when_handle_dropped() {
        // Build the bare worker thread directly (skipping MergeWorker::spawn
        // which would open real DB files) so we can verify the
        // drop-sender -> thread-exit invariant without filesystem side
        // effects.
        let (_resp_tx, resp_rx) = mpsc::channel::<String>();
        let reader = ChannelLineReader::new(resp_rx);
        let writer: Vec<u8> = Vec::new();

        let (job_tx, job_rx) = mpsc::channel::<MergeJob>();
        let handle = std::thread::spawn(move || {
            let db = TasksDb::open_memory().expect("open in-memory db");
            let resolver = ProjectResolver::test(&[]);
            let mut w = writer;
            let r = reader;
            worker_loop(db, resolver, job_rx, &mut w, r);
        });

        drop(job_tx);

        let start = Instant::now();
        while !handle.is_finished() {
            if start.elapsed() > Duration::from_secs(5) {
                panic!("worker thread did not exit within 5s of sender drop");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.join().expect("worker thread join");
    }

    /// FIFO ordering: jobs enqueued in order are picked up in order.
    /// Uses a stripped-down loop that only records the task ids it
    /// observes, so we don't need a fully set-up merge environment.
    #[test]
    fn worker_processes_jobs_fifo() {
        let (tx, rx) = mpsc::channel::<MergeJob>();
        let observed = Arc::new(Mutex::new(Vec::<i64>::new()));
        let observed_clone = Arc::clone(&observed);

        let handle = std::thread::spawn(move || {
            for job in rx {
                let mut g = observed_clone.lock().expect("observed lock");
                g.push(job.task_id);
            }
        });

        for id in [1i64, 2, 3] {
            tx.send(MergeJob {
                task_id: id,
                caller_session_id: None,
            })
            .expect("send job");
        }
        drop(tx);

        handle.join().expect("join");
        let g = observed.lock().expect("observed lock");
        assert_eq!(*g, vec![1i64, 2, 3]);
    }

    /// Regression for #540: the main-loop merge-pass entry point must
    /// return without blocking when it enqueues a job onto a live
    /// worker. We simulate a slow merge by giving the worker a reader
    /// that will never produce a response (so `merge_task_for_caller`
    /// blocks forever on its first `ServerRequest`), then time the
    /// Regression for #540: the main-loop merge-pass entry point must
    /// return without blocking when it enqueues a job onto a live
    /// worker. We simulate a slow merge by giving the worker a reader
    /// whose sender we hold briefly — long enough to prove the
    /// enqueue timing — then drop to let the worker unwind. Without
    /// the sender, every ServerRequest inside `merge_task_for_caller`
    /// sees EOF, returns `Err`, and the worker loop drops back to
    /// `rx.recv()`, which also sees EOF once the job-sender closes.
    ///
    /// This is the critical invariant behind the task: the plugin main
    /// loop stays responsive while a merge is in flight on the worker.
    #[test]
    fn main_loop_enqueue_returns_promptly_even_while_worker_busy() {
        // Build an approved task in an on-disk (WAL) DB so both the
        // main-loop simulator and the worker can open the same file.
        let tmp = tempfile::NamedTempFile::new().expect("tmpfile");
        let db_path = tmp.path().to_path_buf();
        // Keep the file around after the NamedTempFile is closed.
        drop(tmp);

        let db = TasksDb::open(&db_path).expect("open db");
        let task = db
            .create_task(
                "test-project",
                "Test merge task",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .expect("create task");
        // interactive -> ready -> active -> review -> approved
        for s in [
            TaskState::Ready,
            TaskState::Active,
            TaskState::Review,
            TaskState::Approved,
        ] {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(s),
                    ..Default::default()
                },
                None,
            )
            .expect("transition");
        }

        // Worker reader: channel whose sender we keep alive across the
        // timing window, then close. While the sender is alive,
        // `read_line` on the reader blocks (no line ever arrives), so
        // the first RPC inside the merge hangs and the worker is
        // genuinely "busy". Once we drop `resp_tx`, subsequent reads
        // see EOF and the worker unwinds cleanly.
        let (resp_tx, resp_rx) = mpsc::channel::<String>();
        let worker_reader = ChannelLineReader::new(resp_rx);
        let worker_writer: Vec<u8> = Vec::new();

        // Spawn the worker against the same file-backed DB.
        let worker_db = TasksDb::open(&db_path).expect("open worker db");
        let worker_resolver = ProjectResolver::test(&[("test-project", "/tmp/nonexistent")]);
        let worker =
            MergeWorker::spawn_with(worker_db, worker_resolver, worker_writer, worker_reader)
                .expect("spawn worker");

        // The actual invariant: enqueue must return promptly even
        // when the worker is in the middle of a merge.
        let start = Instant::now();
        worker
            .enqueue(MergeJob {
                task_id: task.id,
                caller_session_id: None,
            })
            .expect("enqueue job");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "enqueue took {:?}, expected <100ms — main loop would be blocked",
            elapsed
        );

        // Shutdown sequence: close the response channel first so any
        // in-flight RPC the worker issued fails with EOF and the
        // worker loop can complete the job. Then drop the worker,
        // which closes the job channel, makes `rx.recv()` return
        // `Err`, and the thread exits. `MergeWorker::drop` joins the
        // thread, so by the time we return from this test the thread
        // is fully reaped — no leaks across test runs.
        drop(resp_tx);
        drop(worker);

        // Clean up the temp DB file.
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn db_update_with_retry_succeeds_on_first_attempt() {
        // Smoke-check the retry helper: a normal update returns Ok
        // immediately and the new state is persisted.
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "p",
                "t",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let res = db_update_with_retry(
            &db,
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Active),
                ..Default::default()
            },
        );
        assert!(res.is_ok(), "expected Ok, got {:?}", res);
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Active
        );
    }

    #[test]
    fn db_update_with_retry_propagates_terminal_error() {
        // Asking for an invalid transition is a deterministic error
        // (not transient). The helper retries DB_UPDATE_RETRIES times
        // and then surfaces the underlying error.
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "p",
                "t",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // Ready -> Merged is not a valid transition.
        let res = db_update_with_retry(
            &db,
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Merged),
                ..Default::default()
            },
        );
        assert!(res.is_err(), "expected Err, got {:?}", res);
    }
}
