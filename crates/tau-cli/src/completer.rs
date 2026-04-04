//! Dynamic shell completion for tau CLI arguments.
//!
//! Each completer connects to the tau server and queries live data.
//! Returns empty list if server is unreachable — completion degrades gracefully.

use clap_complete::CompletionCandidate;
use tau::protocol::{Request, Response, format_tokens};

/// Send a request to the server and collect the response synchronously.
/// Returns None if server is unreachable.
fn query(req: &Request) -> Option<Response> {
    smol::block_on(async {
        let mut client = tau::client::Client::connect().await.ok()?;
        client.send(req).await.ok()?;
        let mut response = None;
        client
            .recv_streaming(|resp| {
                response = Some(resp.clone());
            })
            .await
            .ok()?;
        response
    })
}

/// Complete session IDs (with model + message count as help text).
pub fn session_completer() -> Vec<CompletionCandidate> {
    let Some(Response::Sessions { sessions }) = query(&Request::ListSessions {
        include_archived: false,
    }) else {
        return vec![];
    };
    sessions
        .into_iter()
        .map(|s| {
            let help = format!("{}/{} {} msgs", s.provider, s.model, s.message_count);
            CompletionCandidate::new(s.id).help(Some(help.into()))
        })
        .collect()
}

/// Complete model IDs (with provider + context window as help text).
pub fn model_completer() -> Vec<CompletionCandidate> {
    let Some(Response::Models { models }) = query(&Request::ListModels) else {
        return vec![];
    };
    models
        .into_iter()
        .map(|m| {
            let help = format!("{} {}ctx", m.provider, format_tokens(m.context_window));
            CompletionCandidate::new(m.id).help(Some(help.into()))
        })
        .collect()
}

/// Complete archived session IDs (for the restore command).
pub fn archived_session_completer() -> Vec<CompletionCandidate> {
    let Some(Response::Sessions { sessions }) = query(&Request::ListSessions {
        include_archived: true,
    }) else {
        return vec![];
    };
    sessions
        .into_iter()
        .filter(|s| s.archived)
        .map(|s| {
            let help = format!(
                "[archived] {}/{} {} msgs",
                s.provider, s.model, s.message_count
            );
            CompletionCandidate::new(s.id).help(Some(help.into()))
        })
        .collect()
}
