//! Model id / alias → concrete `Model` resolver.
//!
//! The resolver is intentionally pure (no I/O, no locks) so that callers
//! can compose it with their own state ownership.  Both `create_session`
//! and `set_model` flow through it.
//!
//! ## Resolution order
//!
//! 1. **Project alias map** (if provided): if `raw` matches a key here, the
//!    target is taken from the project map.
//! 2. **Global alias map**: same lookup.
//! 3. **Literal model id**: `raw` is treated as a model id directly.
//!
//! At most one alias hop is performed: alias targets must be model ids
//! (optionally `provider/model-id`), never another alias.  This makes
//! cycles impossible by construction.
//!
//! ## Alias collisions
//!
//! If an alias has the same name as a real model id, the alias wins.
//! This is documented in `docs/CONFIG.md`.
//!
//! ## `provider/model-id` parsing
//!
//! Alias targets may be prefixed with a provider name and a `/`.  We split
//! on the **first** `/` only, so `"foo/bar/baz"` is parsed as
//! `provider="foo"`, `id="bar/baz"`.  This lets unusual model ids that
//! contain slashes still resolve.
//!
//! When the caller provides an explicit `provider_filter` (the
//! `provider_name` argument from a request), it always takes precedence
//! over a `provider/` prefix in the alias target — matching the existing
//! behavior of the (Some(model_id), Some(provider)) request branch.

use std::collections::HashMap;

use crate::types::Model;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by [`resolve_model`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// `raw` matched an alias but the alias target does not point to any
    /// known model.  Always an error: surfaces config bugs to the user
    /// instead of silently falling back to the default.
    UnknownAlias {
        /// The alias name the caller passed in.
        name: String,
        /// The target string the alias was pointing at.
        target: String,
        /// Where the alias came from: `"project"` or `"global"`.
        scope: &'static str,
    },
    /// `raw` was not an alias and was not a known model id.  Callers may
    /// choose to surface this as an error or fall back to a default.
    UnknownModel { name: String },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::UnknownAlias {
                name,
                target,
                scope,
            } => write!(
                f,
                "{} alias '{}' points at unknown model '{}'",
                scope, name, target
            ),
            ResolveError::UnknownModel { name } => {
                write!(f, "unknown model or alias: {}", name)
            }
        }
    }
}

impl std::error::Error for ResolveError {}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Resolve a `model_id`-shaped string (possibly an alias) to a concrete
/// `Model` reference from `all_models`.
///
/// See the module docs for the lookup order and disambiguation rules.
pub fn resolve_model<'a>(
    raw: &str,
    provider_filter: Option<&str>,
    project_aliases: Option<&HashMap<String, String>>,
    global_aliases: &HashMap<String, String>,
    all_models: &'a [Model],
) -> Result<&'a Model, ResolveError> {
    // 1+2. Alias lookup (project takes precedence over global).
    let (target, alias_scope): (&str, Option<&'static str>) = if let Some(map) =
        project_aliases.and_then(|m| if m.is_empty() { None } else { Some(m) })
        && let Some(t) = map.get(raw)
    {
        (t.as_str(), Some("project"))
    } else if let Some(t) = global_aliases.get(raw) {
        (t.as_str(), Some("global"))
    } else {
        (raw, None)
    };

    // 3. Parse `provider/id` form. Split on the FIRST `/` only so that
    //    model ids containing slashes are preserved in the id half.
    let (target_provider, target_id) = match target.split_once('/') {
        Some((p, i)) if !p.is_empty() && !i.is_empty() => (Some(p), i),
        _ => (None, target),
    };

    // 4. Combine with the explicit provider_filter from the request.
    //    Explicit filter wins over the alias-target's prefix.
    let effective_provider: Option<&str> = provider_filter.or(target_provider);

    // 5. Look up in all_models.
    let found = all_models.iter().find(|m| {
        m.id == target_id
            && match effective_provider {
                Some(p) => m.provider == p,
                None => true,
            }
    });

    match found {
        Some(m) => Ok(m),
        None => match alias_scope {
            Some(scope) => Err(ResolveError::UnknownAlias {
                name: raw.to_string(),
                target: target.to_string(),
                scope,
            }),
            None => Err(ResolveError::UnknownModel {
                name: raw.to_string(),
            }),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Model, ModelCost, ThinkingStyle};

    fn mk_model(id: &str, provider: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: "mock".into(),
            provider: provider.into(),
            base_url: "http://mock".into(),
            thinking: ThinkingStyle::None,
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4_096,
            headers: std::collections::HashMap::new(),
        }
    }

    fn make_models() -> Vec<Model> {
        vec![
            mk_model("opus-4", "anthropic"),
            mk_model("haiku-4", "anthropic"),
            mk_model("gpt-4", "openai"),
            // Same id under two providers, used to test the provider filter.
            mk_model("dual", "anthropic"),
            mk_model("dual", "openai"),
        ]
    }

    #[test]
    fn plain_id_pass_through() {
        let models = make_models();
        let aliases = HashMap::new();
        let m = resolve_model("opus-4", None, None, &aliases, &models).unwrap();
        assert_eq!(m.id, "opus-4");
        assert_eq!(m.provider, "anthropic");
    }

    #[test]
    fn unknown_plain_id_returns_unknown_model() {
        let models = make_models();
        let aliases = HashMap::new();
        let err = resolve_model("nope", None, None, &aliases, &models).unwrap_err();
        assert_eq!(
            err,
            ResolveError::UnknownModel {
                name: "nope".into()
            }
        );
    }

    #[test]
    fn global_alias_hit() {
        let models = make_models();
        let mut aliases = HashMap::new();
        aliases.insert("smart".into(), "opus-4".into());
        let m = resolve_model("smart", None, None, &aliases, &models).unwrap();
        assert_eq!(m.id, "opus-4");
    }

    #[test]
    fn project_alias_hit() {
        let models = make_models();
        let global = HashMap::new();
        let mut project = HashMap::new();
        project.insert("smart".into(), "haiku-4".into());
        let m = resolve_model("smart", None, Some(&project), &global, &models).unwrap();
        assert_eq!(m.id, "haiku-4");
    }

    #[test]
    fn project_overrides_global() {
        let models = make_models();
        let mut global = HashMap::new();
        global.insert("smart".into(), "opus-4".into());
        let mut project = HashMap::new();
        project.insert("smart".into(), "haiku-4".into());
        let m = resolve_model("smart", None, Some(&project), &global, &models).unwrap();
        assert_eq!(m.id, "haiku-4");
    }

    #[test]
    fn unknown_alias_target_is_error() {
        let models = make_models();
        let mut global = HashMap::new();
        global.insert("smart".into(), "ghost".into());
        let err = resolve_model("smart", None, None, &global, &models).unwrap_err();
        assert_eq!(
            err,
            ResolveError::UnknownAlias {
                name: "smart".into(),
                target: "ghost".into(),
                scope: "global",
            }
        );
    }

    #[test]
    fn unknown_project_alias_target_reports_project_scope() {
        let models = make_models();
        let global = HashMap::new();
        let mut project = HashMap::new();
        project.insert("planner".into(), "ghost".into());
        let err = resolve_model("planner", None, Some(&project), &global, &models).unwrap_err();
        assert!(matches!(
            err,
            ResolveError::UnknownAlias {
                ref name,
                ref target,
                scope: "project",
            } if name == "planner" && target == "ghost"
        ));
    }

    #[test]
    fn provider_prefixed_alias_target() {
        let models = make_models();
        let mut global = HashMap::new();
        global.insert("dual_a".into(), "anthropic/dual".into());
        global.insert("dual_o".into(), "openai/dual".into());
        let a = resolve_model("dual_a", None, None, &global, &models).unwrap();
        assert_eq!(a.provider, "anthropic");
        let o = resolve_model("dual_o", None, None, &global, &models).unwrap();
        assert_eq!(o.provider, "openai");
    }

    #[test]
    fn explicit_provider_filter_overrides_alias_prefix() {
        let models = make_models();
        let mut global = HashMap::new();
        // Alias points at anthropic/dual, but request asks for openai.
        global.insert("d".into(), "anthropic/dual".into());
        let m = resolve_model("d", Some("openai"), None, &global, &models).unwrap();
        assert_eq!(m.provider, "openai");
        assert_eq!(m.id, "dual");
    }

    #[test]
    fn explicit_provider_filter_on_plain_id() {
        let models = make_models();
        let global = HashMap::new();
        let m = resolve_model("dual", Some("openai"), None, &global, &models).unwrap();
        assert_eq!(m.provider, "openai");
    }

    #[test]
    fn explicit_provider_filter_no_match_is_error() {
        let models = make_models();
        let global = HashMap::new();
        // 'gpt-4' only exists under provider 'openai'; asking for 'anthropic'
        // must fail rather than returning the openai one.
        let err = resolve_model("gpt-4", Some("anthropic"), None, &global, &models).unwrap_err();
        assert_eq!(
            err,
            ResolveError::UnknownModel {
                name: "gpt-4".into()
            }
        );
    }

    #[test]
    fn alias_name_matches_real_model_id_alias_wins() {
        let models = make_models();
        // 'opus-4' is a real model id. We add an alias of the same name
        // that points elsewhere — alias must win.
        let mut global = HashMap::new();
        global.insert("opus-4".into(), "haiku-4".into());
        let m = resolve_model("opus-4", None, None, &global, &models).unwrap();
        assert_eq!(m.id, "haiku-4");
    }

    #[test]
    fn split_on_first_slash_only() {
        // A made-up model id containing a slash, registered under
        // provider "foo".  Alias target is "foo/bar/baz", which should
        // resolve to provider="foo", id="bar/baz".
        let mut models = make_models();
        models.push(mk_model("bar/baz", "foo"));
        let mut global = HashMap::new();
        global.insert("weird".into(), "foo/bar/baz".into());
        let m = resolve_model("weird", None, None, &global, &models).unwrap();
        assert_eq!(m.id, "bar/baz");
        assert_eq!(m.provider, "foo");
    }

    #[test]
    fn empty_alias_maps_match_plain_lookup() {
        // Regression: with both maps empty the resolver behaves identically
        // to a direct `all_models.iter().find` over (id, provider?).
        let models = make_models();
        let global = HashMap::new();
        let project = HashMap::new();

        // Plain id, no provider — finds first match.
        let m = resolve_model("opus-4", None, Some(&project), &global, &models).unwrap();
        assert_eq!(m.id, "opus-4");

        // Plain id + explicit provider.
        let m = resolve_model("dual", Some("openai"), Some(&project), &global, &models).unwrap();
        assert_eq!(m.provider, "openai");

        // Unknown id with empty maps still returns UnknownModel (not UnknownAlias).
        let err = resolve_model("ghost", None, Some(&project), &global, &models).unwrap_err();
        assert_eq!(
            err,
            ResolveError::UnknownModel {
                name: "ghost".into()
            }
        );
    }

    #[test]
    fn empty_project_map_falls_through_to_global() {
        // An empty Some(&project) map should not shadow the global lookup.
        let models = make_models();
        let mut global = HashMap::new();
        global.insert("smart".into(), "opus-4".into());
        let project = HashMap::new();
        let m = resolve_model("smart", None, Some(&project), &global, &models).unwrap();
        assert_eq!(m.id, "opus-4");
    }
}
