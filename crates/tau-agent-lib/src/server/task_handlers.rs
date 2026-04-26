//! Task protocol handlers — used by both client dispatch and plugin dispatch.

use std::collections::{HashMap, HashSet};

use crate::protocol::{
    Response, TaskHistoryInfo, TaskInfo, TaskMessageInfo, TaskRelationInfo, TaskSessionInfo,
};

use super::dispatch::get_session_info_impl;
use super::state::{SharedState, lock_state};

fn task_to_info(t: crate::tasks_db::Task) -> TaskInfo {
    TaskInfo {
        id: t.id,
        project_name: t.project_name,
        title: t.title,
        state: t.state.as_str().to_string(),
        priority: t.priority,
        parent_id: t.parent_id,
        tags: t.tags,
        affected_files: t.affected_files,
        branch: t.branch,
        worktree_path: t.worktree_path,
        session_id: t.session_id,
        skip_review: t.skip_review,
        require_approval: t.require_approval,
        sandbox_profile: t.sandbox_profile,
        held: t.held,
        has_live_session: false,
        filed_by_project: t.filed_by_project,
        filed_by_session_id: t.filed_by_session_id,
        created_at: t.created_at,
        updated_at: t.updated_at,
    }
}

/// Convert a DB task to its wire form, setting `has_live_session` by
/// intersecting the task's recorded sessions with `live_task_ids`.
fn task_to_info_with_live(t: crate::tasks_db::Task, live_task_ids: &HashSet<i64>) -> TaskInfo {
    let mut info = task_to_info(t);
    if live_task_ids.contains(&info.id) {
        info.has_live_session = true;
    }
    info
}

/// Compute the set of task ids in `project` that currently have at least one
/// live (actively-running) session.  Intersects `task_sessions` rows with the
/// server's `live_sessions` set.
fn live_task_ids_for_project(state: &SharedState, project: &str) -> HashSet<i64> {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(_) => return HashSet::new(),
    };
    let rows = db.list_project_task_sessions(project).unwrap_or_default();
    if rows.is_empty() {
        return HashSet::new();
    }
    // Bucket sessions-by-task so we don't lock shared state once per row.
    let mut by_task: HashMap<i64, Vec<String>> = HashMap::new();
    for (task_id, sid) in rows {
        by_task.entry(task_id).or_default().push(sid);
    }
    let st = lock_state(state);
    by_task
        .into_iter()
        .filter_map(|(task_id, sids)| {
            if sids.iter().any(|s| st.live_sessions.contains(s)) {
                Some(task_id)
            } else {
                None
            }
        })
        .collect()
}

fn msg_to_info(m: crate::tasks_db::TaskMessage) -> TaskMessageInfo {
    TaskMessageInfo {
        id: m.id,
        task_id: m.task_id,
        content: m.content,
        author: m.author,
        created_at: m.created_at,
        updated_at: m.updated_at,
    }
}

fn rel_to_info(r: crate::tasks_db::TaskRelation) -> TaskRelationInfo {
    TaskRelationInfo {
        from_task: r.from_task,
        to_task: r.to_task,
        relation: r.relation,
    }
}

fn open_tasks_db() -> Result<crate::tasks_db::TasksDb, Response> {
    crate::tasks_db::TasksDb::open_default().map_err(|e| Response::Error {
        message: e.to_string(),
    })
}

pub(super) fn handle_task_list(
    state: &SharedState,
    project: &str,
    state_filter: Option<&str>,
    parent_id: Option<i64>,
) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    match db.list_tasks(project, state_filter, parent_id, None, None) {
        Ok(tasks) => {
            let tree = crate::tasks_db::tree_order(tasks);
            let live = live_task_ids_for_project(state, project);
            Response::TaskTree {
                tasks: tree
                    .into_iter()
                    .map(|(d, t)| (d, task_to_info_with_live(t, &live)))
                    .collect(),
            }
        }
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

pub(super) fn handle_task_get(state: &SharedState, id: i64) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    let task = match db.get_task(id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Response::Error {
                message: format!("task {} not found", id),
            };
        }
        Err(e) => {
            return Response::Error {
                message: e.to_string(),
            };
        }
    };
    let messages = match db.get_messages(id) {
        Ok(m) => m,
        Err(e) => {
            return Response::Error {
                message: format!("failed to get messages for task {}: {}", id, e),
            };
        }
    };
    let relations = match db.get_relations(id) {
        Ok(r) => r,
        Err(e) => {
            return Response::Error {
                message: format!("failed to get relations for task {}: {}", id, e),
            };
        }
    };
    let subtasks = match db.get_subtasks(id) {
        Ok(s) => s,
        Err(e) => {
            return Response::Error {
                message: format!("failed to get subtasks for task {}: {}", id, e),
            };
        }
    };
    let task_sessions = db.get_sessions(id).unwrap_or_default();
    let mut sessions: Vec<TaskSessionInfo> = Vec::with_capacity(task_sessions.len());
    for ts in task_sessions {
        // Enrich with a best-effort GetSessionInfo-style lookup.  Sessions
        // that can't be resolved (deleted, other store, etc.) are dropped
        // from the enriched view — callers that want the raw row can query
        // `get_sessions` directly.
        match get_session_info_impl(state, &ts.session_id) {
            Response::SessionInfo { info } => {
                sessions.push(TaskSessionInfo {
                    session_id: ts.session_id,
                    role: ts.role,
                    created_at: ts.created_at,
                    message_count: Some(info.message_count),
                    archived: Some(info.archived),
                    last_activity: Some(info.last_activity),
                    last_phase: Some(info.state),
                    last_exit_status: info.last_exit_status,
                    is_live: info.is_live,
                });
            }
            _ => {
                // Session no longer exists — drop the row, per spec.
            }
        }
    }
    let history: Vec<TaskHistoryInfo> = db
        .get_history(id)
        .unwrap_or_default()
        .into_iter()
        .map(|h| TaskHistoryInfo {
            field: h.field,
            old_value: h.old_value,
            new_value: h.new_value,
            session_id: h.session_id,
            created_at: h.created_at,
        })
        .collect();

    // Populate has_live_session from the sessions we just enriched, and for
    // subtasks via a per-project lookup.
    let mut task_info = task_to_info(task);
    task_info.has_live_session = sessions.iter().any(|s| s.is_live);
    let live_for_project = live_task_ids_for_project(state, &task_info.project_name);
    Response::TaskDetail {
        task: task_info,
        messages: messages.into_iter().map(msg_to_info).collect(),
        relations: relations.into_iter().map(rel_to_info).collect(),
        subtasks: subtasks
            .into_iter()
            .map(|t| task_to_info_with_live(t, &live_for_project))
            .collect(),
        sessions,
        history,
    }
}

pub fn handle_task_create(
    project: &str,
    title: &str,
    parent_id: Option<i64>,
    priority: Option<i32>,
    tags: &[String],
    sandbox_profile: Option<&str>,
) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    let tags_val = if tags.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(
            tags.iter()
                .map(|t| serde_json::Value::String(t.clone()))
                .collect(),
        ))
    };
    match db.create_task(
        project,
        title,
        priority.map(|p| p as i64),
        parent_id,
        tags_val.as_ref(),
        false,
        "planning",
        false,
        None,
        sandbox_profile,
        false,
        None,
        false,
        crate::tasks_db::FiledBy::default(),
    ) {
        Ok(task) => Response::TaskUpdated {
            task: task_to_info(task),
        },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub fn handle_task_update(
    id: i64,
    new_state: Option<String>,
    title: Option<String>,
    priority: Option<i64>,
    tags: Option<serde_json::Value>,
    affected_files: Option<serde_json::Value>,
    skip_review: Option<bool>,
    require_approval: Option<bool>,
    sandbox_profile: Option<String>,
) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    let new_state = match new_state {
        None => None,
        Some(s) => match crate::tasks_state::TaskState::from_db_str(&s) {
            Ok(st) => Some(st),
            Err(_) => {
                return Response::Error {
                    message: format!("invalid task state '{}'", s),
                };
            }
        },
    };
    let update = crate::tasks_db::TaskUpdate {
        state: new_state,
        title,
        priority,
        tags,
        affected_files,
        skip_review,
        require_approval,
        merge_target: None,
        sandbox_profile,
        held: None,
        project_name: None,
    };
    match db.update_task(id, &update, None) {
        Ok(task) => Response::TaskUpdated {
            task: task_to_info(task),
        },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

pub fn handle_task_search(project: &str, query: &str, state_filter: Option<&str>) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    match db.search_tasks(project, query, state_filter) {
        Ok(tasks) => Response::TaskList {
            tasks: tasks.into_iter().map(task_to_info).collect(),
        },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

pub fn handle_task_assign(id: i64, session_id: &str) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    match db.assign_task(id, session_id) {
        Ok(result) => Response::TaskUpdated {
            task: task_to_info(result.task),
        },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

pub fn handle_task_status(project: &str) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    match crate::tasks_scheduler::get_status(&db, project, None) {
        Ok(status) => {
            let text = crate::tasks_scheduler::format_status(&status);
            Response::TaskStatus { text }
        }
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

pub(super) fn handle_task_overview(
    state: &SharedState,
    project: &str,
    recent_limit: usize,
) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    let live = live_task_ids_for_project(state, project);
    match crate::tasks_scheduler::task_overview_response(&db, project, recent_limit, &live) {
        Ok(resp) => resp,
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

pub fn handle_task_merge_queue(project: &str) -> Response {
    let db = match open_tasks_db() {
        Ok(db) => db,
        Err(resp) => return resp,
    };
    let approved = match db.list_tasks(project, Some("approved"), None, None, None) {
        Ok(t) => t,
        Err(e) => {
            return Response::Error {
                message: e.to_string(),
            };
        }
    };
    let merging = match db.list_tasks(project, Some("merging"), None, None, None) {
        Ok(t) => t,
        Err(e) => {
            return Response::Error {
                message: e.to_string(),
            };
        }
    };
    let tasks: Vec<TaskInfo> = approved
        .into_iter()
        .chain(merging)
        .map(task_to_info)
        .collect();
    Response::TaskMergeQueue { tasks }
}
