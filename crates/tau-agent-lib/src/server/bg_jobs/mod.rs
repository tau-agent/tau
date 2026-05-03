//! Concrete background-job implementations registered against the
//! [`super::bg_tasks::BgTaskScheduler`].
//!
//! Each submodule defines one [`super::bg_tasks::BgJob`] plus its
//! configuration knobs.  Server startup wires them up in
//! [`super::run`] / [`super::run_with_config`].

pub(crate) mod gc_empty_sessions;
pub(crate) mod refresh_subscription_usage;
