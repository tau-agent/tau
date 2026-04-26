//! Plugin RPC services for the myelin-based plugin transport.
//!
//! This module is the *successor* to `plugin_protocol.rs`: where that module
//! describes a hand-rolled JSON-lines protocol with two enums
//! (`PluginRequest` / `PluginMessage`) routed by string-keyed tunnels, this
//! module defines the same wire as **two myelin services** running over a
//! single [`DuplexStreamTransport`](myelin::stream::DuplexStreamTransport)
//! per plugin subprocess.
//!
//! Both peers — server (`tau-agent-lib`) and plugin subprocess
//! (`tau-agent-plugin-worker` / `tau-agent-plugin-tasks`) — *call* and
//! *serve* methods. Direction is encoded in the API id.
//!
//! ## Wire layout
//!
//! Each framed payload is `[u8 kind][u16 api_id LE][u8 slot_id][CBOR bytes]`,
//! length-prefixed. The two API ids in use are [`PLUGIN_API_ID`] (server
//! calls plugin) and [`PLUGIN_CALLBACK_API_ID`] (plugin calls server) —
//! both emitted by the `#[myelin::service]` attribute on the trait
//! definitions below.
//!
//! ## Codec
//!
//! [`CborCodec`](myelin::stream::CborCodec). Self-describing, debuggable
//! with `xxd`/CBOR pretty-printers. Postcard would be smaller but loses
//! debuggability.
//!
//! ## Status
//!
//! The trait definitions and value types live here; the actual transport
//! plumbing in `tau-agent-lib::plugin` and the per-plugin executor crates
//! still uses the JSON-lines protocol from `plugin_protocol.rs`. The
//! migration replaces one binary at a time. See task #759.

// Myelin services use plain `async fn` in trait definitions — not boxed
// futures — so we tolerate the auto-trait-bound lint the same way myelin
// itself does.
#![allow(async_fn_in_trait)]

use serde::{Deserialize, Serialize};

use crate::plugin_protocol::{HookResult, PluginRegistration, PluginToolResult};
use crate::protocol::{Request, Response};

// ---------------------------------------------------------------------------
// API ids
// ---------------------------------------------------------------------------
//
// Both `#[myelin::service]` attributes below pin their `api_id` explicitly
// (rather than letting the macro derive an FNV hash of the trait name). This
// makes the wire stable across trait renames. The attribute emits public
// `PLUGIN_API_ID` and `PLUGIN_CALLBACK_API_ID` constants — use those.

// ---------------------------------------------------------------------------
// Value types — request payloads grouped into structs so the macro-generated
// enum variants stay tidy. (`#[myelin::service]` would otherwise turn each
// trait method's parameter list into the variant fields, which is fine for
// trivial methods but noisy for the bigger ones.)
// ---------------------------------------------------------------------------

/// Context passed at plugin initialization (`init`) and on every
/// `session_start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCtx {
    /// Working directory for the session.
    pub cwd: String,
    /// Session id.
    pub session_id: String,
    /// Project name for this session, if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_name: Option<String>,
}

/// A tool call dispatch from the server to the plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallReq {
    /// Unique id for this tool call (used to correlate cancel/output_delta).
    pub tool_call_id: String,
    /// Tool name (must match a name from the plugin's registration).
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: serde_json::Value,
    /// Working directory for tool execution.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cwd: Option<String>,
    /// Session id this tool call belongs to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    /// Project name for this session.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_name: Option<String>,
}

/// A hook invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookReq {
    /// Hook name (e.g. `"before_llm_turn"`, `"after_tool_result"`).
    pub name: String,
    /// Hook-specific payload.
    pub data: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Service traits
// ---------------------------------------------------------------------------

/// Methods the **server** calls on the **plugin**.
///
/// The plugin process serves this trait; the server holds a generated
/// `PluginClient` over a `DuplexClientHalf<_, _, …, PluginRequest,
/// PluginResponse>` bound to [`PLUGIN_API_ID`].
///
/// Concurrent in-flight requests are first-class: `tool_call` and
/// `cancel_tool_call` may overlap (in fact `cancel_tool_call` is *only*
/// useful while a `tool_call` is pending — its purpose is to abort it).
/// Myelin's `MuxedSlots` handles the slot routing for free; the plugin
/// must serve them concurrently (e.g. spawn tool_call as a background task
/// so the dispatch loop can pick up the cancel).
#[myelin::service(api_id = 0x0001)]
pub trait PluginService {
    /// Initialise the plugin with session context. Sent once after the
    /// plugin has called `register` on the server side.
    async fn init(&self, ctx: SessionCtx);

    /// Execute a hook (e.g. `before_llm_turn`).
    async fn hook(&self, req: HookReq) -> HookResult;

    /// Execute a tool call. Returns the final result; intermediate output
    /// is reported via `output_delta` on the [`PluginCallbackService`]
    /// service.
    async fn tool_call(&self, call: ToolCallReq) -> PluginToolResult;

    /// Abort a tool call by id. The plugin should kill any associated
    /// subprocess and return a normal `tool_call` response with
    /// cancellation noted. No-op if the call already completed.
    async fn cancel_tool_call(&self, tool_call_id: String);

    /// Notify the plugin that a (sub-)session is starting.
    async fn session_start(&self, ctx: SessionCtx);

    /// Notify the plugin it has been idle long enough to consider exiting.
    async fn idle(&self);
}

/// Methods the **plugin** calls on the **server**.
///
/// The server serves this trait; the plugin holds a generated
/// `PluginCallbackClient` over a `DuplexClientHalf<_, _, …,
/// PluginCallbackRequest, PluginCallbackResponse>` bound to
/// [`PLUGIN_CALLBACK_API_ID`].
///
/// `output_delta` deserves a note: the existing protocol fires it
/// one-way (no reply expected) at up to ~200 events/sec during a long
/// bash command. Myelin's RPC model is request/response, so we keep it
/// as a `()`-returning RPC and *do* await it. This costs an extra
/// response frame per delta but bounds in-flight deltas at the slot
/// pool size, preserving the protocol's flow-control properties. A
/// future myelin streaming feature could collapse this to a true
/// fire-and-forget notification.
#[myelin::service(api_id = 0x0002)]
pub trait PluginCallbackService {
    /// Plugin registration. Sent once at startup before serving any
    /// `PluginCalledByServer` methods. The server waits for this call
    /// before considering the plugin ready.
    async fn register(&self, reg: PluginRegistration);

    /// Forward a [`Request`] from the plugin to the server's main
    /// request handler (the same one the TUI/CLI talks to).
    async fn server_request(&self, req: Request) -> Response;

    /// Streaming tool output delta. Plugin → server, fire-then-await.
    /// See trait-level note on the `()` return.
    async fn output_delta(&self, tool_call_id: String, text: String);
}

// ---------------------------------------------------------------------------
// Default duplex transport configuration
// ---------------------------------------------------------------------------

/// Number of concurrent in-flight outgoing RPCs per direction.
///
/// 32 is comfortably above the worst-case fan-out we expect:
/// - 1 in-flight `tool_call` per plugin (serialised by
///   `PluginHandle::take_tool_plugin` today).
/// - bursts of `output_delta` from a long-running tool.
/// - a `cancel_tool_call` racing the in-flight `tool_call`.
/// - the merge-worker thread inside the tasks plugin issuing a
///   `server_request` concurrently with the main dispatch loop.
pub const DUPLEX_SLOTS: usize = 32;

/// Per-slot reply buffer, in bytes.
///
/// `PluginToolResult` payloads can be large (a long bash transcript may
/// reach tens of KiB). 128 KiB leaves headroom while staying small enough
/// that `MuxedSlots::new_boxed` keeps the entire slot table on the heap.
pub const DUPLEX_BUF: usize = 131_072;

/// Type alias for the duplex transport tau uses on every plugin pipe.
///
/// `R` / `W` are the plugin's reader/writer halves of the byte stream
/// (typically `FuturesIoReader<Async<File>>` / `FuturesIoWriter<…>` for
/// stdin/stdout pipes).
pub type PluginDuplex<R, W> = myelin::stream::DuplexStreamTransport<
    R,
    W,
    myelin::stream::LengthPrefixed,
    myelin::stream::CborCodec,
    DUPLEX_SLOTS,
    DUPLEX_BUF,
>;

// ---------------------------------------------------------------------------
// Tests — round-trip a single RPC over a UnixStream pair, with both halves
// of both services running concurrently. This is the M2 smoke test from
// task #759: it proves the macro expansion, codec, framer, transport, and
// pump all wire together correctly on the runtime tau actually uses (smol).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use myelin::io::futures_io::{FuturesIoReader, FuturesIoWriter};
    use myelin::transport::ServerTransport;
    use smol::Async;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn make_pair() -> (
        FuturesIoReader<Async<UnixStream>>,
        FuturesIoWriter<Async<UnixStream>>,
        FuturesIoReader<Async<UnixStream>>,
        FuturesIoWriter<Async<UnixStream>>,
    ) {
        let (sa, sb) = UnixStream::pair().expect("UnixStream::pair");
        sa.set_nonblocking(true).expect("nonblocking sa");
        sb.set_nonblocking(true).expect("nonblocking sb");

        // Each side: clone the FD so we have independent reader+writer
        // halves (the futures_io adapter needs distinct objects for read
        // and write).
        let sa_w = sa.try_clone().expect("clone sa");
        sa_w.set_nonblocking(true).expect("nonblocking sa_w");
        let sb_w = sb.try_clone().expect("clone sb");
        sb_w.set_nonblocking(true).expect("nonblocking sb_w");

        let sa_r = Async::new(sa).expect("Async sa");
        let sa_w = Async::new(sa_w).expect("Async sa_w");
        let sb_r = Async::new(sb).expect("Async sb");
        let sb_w = Async::new(sb_w).expect("Async sb_w");

        (
            FuturesIoReader::new(sa_r),
            FuturesIoWriter::new(sa_w),
            FuturesIoReader::new(sb_r),
            FuturesIoWriter::new(sb_w),
        )
    }

    /// Trivial server impls that count calls; we only need to prove the
    /// generated dispatch + transport plumbing wires up correctly.
    struct PluginSide {
        init_calls: Arc<AtomicU32>,
        idle_calls: Arc<AtomicU32>,
    }

    impl PluginService for PluginSide {
        async fn init(&self, _ctx: SessionCtx) {
            self.init_calls.fetch_add(1, Ordering::SeqCst);
        }
        async fn hook(&self, _req: HookReq) -> HookResult {
            HookResult::default()
        }
        async fn tool_call(&self, call: ToolCallReq) -> PluginToolResult {
            PluginToolResult {
                tool_call_id: call.tool_call_id,
                content: vec![],
                is_error: false,
                summary: None,
                post_persist_actions: vec![],
            }
        }
        async fn cancel_tool_call(&self, _id: String) {}
        async fn session_start(&self, _ctx: SessionCtx) {}
        async fn idle(&self) {
            self.idle_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ServerSide {
        register_calls: Arc<AtomicU32>,
    }

    impl PluginCallbackService for ServerSide {
        async fn register(&self, _reg: PluginRegistration) {
            self.register_calls.fetch_add(1, Ordering::SeqCst);
        }
        async fn server_request(&self, _req: Request) -> Response {
            // Any Response variant works for the round-trip; pick a small one.
            Response::Ok
        }
        async fn output_delta(&self, _id: String, _text: String) {}
    }

    #[test]
    fn duplex_round_trips_both_directions() {
        // Build the duplex transport on each side with the production
        // `PluginDuplex` type alias — this is the exact stack tau will
        // use in M3+.
        let (r_srv, w_srv, r_plg, w_plg) = make_pair();
        let dx_srv: PluginDuplex<_, _> = PluginDuplex::new(r_srv, w_srv);
        let dx_plg: PluginDuplex<_, _> = PluginDuplex::new(r_plg, w_plg);

        // Server: serves PLUGIN_CALLBACK_API_ID (so the plugin can call into us),
        // calls into PLUGIN_API_ID.
        let srv_server = dx_srv
            .server_half::<PluginCallbackRequest, PluginCallbackResponse>(PLUGIN_CALLBACK_API_ID);
        let srv_client = dx_srv.client_half::<PluginRequest, PluginResponse>(PLUGIN_API_ID);

        // Plugin: dual.
        let plg_server = dx_plg.server_half::<PluginRequest, PluginResponse>(PLUGIN_API_ID);
        let plg_client = dx_plg
            .client_half::<PluginCallbackRequest, PluginCallbackResponse>(PLUGIN_CALLBACK_API_ID);

        let (pump_srv, _h_srv) = dx_srv.split();
        let (pump_plg, _h_plg) = dx_plg.split();

        let plugin_init_count = Arc::new(AtomicU32::new(0));
        let plugin_idle_count = Arc::new(AtomicU32::new(0));
        let server_register_count = Arc::new(AtomicU32::new(0));

        let plg_impl = PluginSide {
            init_calls: plugin_init_count.clone(),
            idle_calls: plugin_idle_count.clone(),
        };
        let srv_impl = ServerSide {
            register_calls: server_register_count.clone(),
        };

        smol::block_on(async {
            // Drive a single round trip in each direction.
            let mut plg_server = plg_server;
            let mut srv_server = srv_server;

            let plugin_dispatch = async move {
                // Serve two `init` calls and two `idle` calls.
                for _ in 0..4 {
                    let (req, token) = plg_server.recv().await.expect("plugin recv");
                    let resp = plugin_dispatch(&plg_impl, req).await;
                    plg_server.reply(token, resp).await.expect("plugin reply");
                }
            };
            let server_dispatch = async move {
                // Serve one `register` call.
                let (req, token) = srv_server.recv().await.expect("server recv");
                let resp = plugin_callback_dispatch(&srv_impl, req).await;
                srv_server.reply(token, resp).await.expect("server reply");
            };

            let work = async {
                // Plugin → server: register. The generated client method
                // returns Result<(), TransportError>; in this loopback
                // scenario the transport never fails so we just await it.
                let plg_client = PluginCallbackClient::new(plg_client);
                let _ = plg_client
                    .register(PluginRegistration {
                        name: "smoke".into(),
                        tools: vec![],
                        hooks: vec![],
                        commands: vec![],
                    })
                    .await;

                // Server → plugin: two inits and two idles, concurrently.
                let srv_client = PluginClient::new(srv_client);
                let i1 = srv_client.init(SessionCtx {
                    cwd: "/tmp".into(),
                    session_id: "s1".into(),
                    project_name: None,
                });
                let i2 = srv_client.init(SessionCtx {
                    cwd: "/tmp".into(),
                    session_id: "s2".into(),
                    project_name: None,
                });
                let id1 = srv_client.idle();
                let id2 = srv_client.idle();
                let ((_a, _b), (_c, _d)) = futures_lite::future::zip(
                    futures_lite::future::zip(i1, i2),
                    futures_lite::future::zip(id1, id2),
                )
                .await;
            };

            // Pumps never complete normally; `or` returns when work does.
            futures_lite::future::or(
                async {
                    futures_lite::future::zip(
                        work,
                        futures_lite::future::zip(plugin_dispatch, server_dispatch),
                    )
                    .await;
                },
                async {
                    let _ = futures_lite::future::zip(pump_srv.run(), pump_plg.run()).await;
                },
            )
            .await;
        });

        assert_eq!(plugin_init_count.load(Ordering::SeqCst), 2);
        assert_eq!(plugin_idle_count.load(Ordering::SeqCst), 2);
        assert_eq!(server_register_count.load(Ordering::SeqCst), 1);
    }
}
