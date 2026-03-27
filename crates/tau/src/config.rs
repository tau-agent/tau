//! Provider configuration from `~/.config/tau/providers.toml`.
//!
//! Built-in providers (anthropic, openai) provide defaults.
//! Custom providers and models from TOML are merged on top.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::{Model, ModelCost, ThinkingStyle};

// ---------------------------------------------------------------------------
// Config file types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// API type: "anthropic" or "openai"
    pub api: String,
    pub base_url: String,
    /// Inline API key (or "$ENV_VAR" for env expansion). Optional — can also
    /// come from auth.json or environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    #[serde(default)]
    pub thinking: ThinkingStyle,
    #[serde(default)]
    pub cost: ModelCost,
}

fn default_context_window() -> u64 {
    128_000
}
fn default_max_tokens() -> u64 {
    16_384
}

impl Default for ModelCost {
    fn default() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading & saving
// ---------------------------------------------------------------------------

pub fn config_path() -> PathBuf {
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("tau").join("providers.toml")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join("tau")
            .join("providers.toml")
    } else {
        PathBuf::from("/tmp").join("tau-providers.toml")
    }
}

pub fn load_config() -> crate::Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(e.to_string()))?;
    toml::from_str(&content).map_err(|e| crate::Error::Parse(format!("providers.toml: {}", e)))
}

pub fn save_config(config: &Config) -> crate::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| crate::Error::Io(format!("mkdir: {}", e)))?;
    }
    let content = toml::to_string_pretty(config).map_err(|e| crate::Error::Parse(e.to_string()))?;
    std::fs::write(&path, content).map_err(|e| crate::Error::Io(e.to_string()))
}

// ---------------------------------------------------------------------------
// Resolving: merge built-in + config into final Model list
// ---------------------------------------------------------------------------

/// Resolve API key from config, falling back to `api_key` field with env expansion.
pub fn resolve_provider_api_key(provider_config: &ProviderConfig) -> Option<String> {
    let key = provider_config.api_key.as_deref()?;
    if key == "none" || key.is_empty() {
        return None;
    }
    if let Some(var) = key.strip_prefix('$') {
        return std::env::var(var).ok();
    }
    Some(key.to_string())
}

/// Convert a ModelConfig + provider info into a full Model.
fn model_from_config(provider_name: &str, provider: &ProviderConfig, m: &ModelConfig) -> Model {
    let api = match provider.api.as_str() {
        "anthropic" => "anthropic-messages",
        "openai" => "openai-completions",
        other => other,
    };
    Model {
        id: m.id.clone(),
        name: m.name.clone().unwrap_or_else(|| m.id.clone()),
        api: api.to_string(),
        provider: provider_name.to_string(),
        base_url: provider.base_url.clone(),
        thinking: m.thinking.clone(),
        cost: m.cost.clone(),
        context_window: m.context_window,
        max_tokens: m.max_tokens,
        headers: HashMap::new(),
    }
}

/// Build the complete model list from built-in providers + config file.
/// Config models override built-ins with same (provider, id) pair.
pub fn resolve_models(config: &Config) -> Vec<Model> {
    let mut models: Vec<Model> = Vec::new();

    // Built-in models
    models.extend(crate::providers::anthropic::models());
    models.extend(builtin_openai_models());

    // Custom models from config
    for (provider_name, provider_config) in &config.providers {
        for mc in &provider_config.models {
            let model = model_from_config(provider_name, provider_config, mc);
            // Override if same provider+id exists
            if let Some(existing) = models
                .iter_mut()
                .find(|m| m.provider == model.provider && m.id == model.id)
            {
                *existing = model;
            } else {
                models.push(model);
            }
        }
    }

    models
}

// ---------------------------------------------------------------------------
// Built-in OpenAI models
// ---------------------------------------------------------------------------

fn builtin_openai_models() -> Vec<Model> {
    let api = "openai-completions";
    let provider = "openai";
    let base = "https://api.openai.com/v1";

    vec![
        Model {
            id: "gpt-4.1".into(),
            name: "GPT-4.1".into(),
            api: api.into(),
            provider: provider.into(),
            base_url: base.into(),
            thinking: ThinkingStyle::None,
            cost: ModelCost {
                input: 2.0,
                output: 8.0,
                cache_read: 0.5,
                cache_write: 2.0,
            },
            context_window: 1_048_576,
            max_tokens: 32_768,
            headers: HashMap::new(),
        },
        Model {
            id: "gpt-4.1-mini".into(),
            name: "GPT-4.1 Mini".into(),
            api: api.into(),
            provider: provider.into(),
            base_url: base.into(),
            thinking: ThinkingStyle::None,
            cost: ModelCost {
                input: 0.4,
                output: 1.6,
                cache_read: 0.1,
                cache_write: 0.4,
            },
            context_window: 1_048_576,
            max_tokens: 32_768,
            headers: HashMap::new(),
        },
        Model {
            id: "gpt-4.1-nano".into(),
            name: "GPT-4.1 Nano".into(),
            api: api.into(),
            provider: provider.into(),
            base_url: base.into(),
            thinking: ThinkingStyle::None,
            cost: ModelCost {
                input: 0.1,
                output: 0.4,
                cache_read: 0.025,
                cache_write: 0.1,
            },
            context_window: 1_048_576,
            max_tokens: 32_768,
            headers: HashMap::new(),
        },
        Model {
            id: "o3".into(),
            name: "o3".into(),
            api: api.into(),
            provider: provider.into(),
            base_url: base.into(),
            thinking: ThinkingStyle::OpenAi,
            cost: ModelCost {
                input: 2.0,
                output: 8.0,
                cache_read: 0.5,
                cache_write: 2.0,
            },
            context_window: 200_000,
            max_tokens: 100_000,
            headers: HashMap::new(),
        },
        Model {
            id: "o4-mini".into(),
            name: "o4-mini".into(),
            api: api.into(),
            provider: provider.into(),
            base_url: base.into(),
            thinking: ThinkingStyle::OpenAi,
            cost: ModelCost {
                input: 1.1,
                output: 4.4,
                cache_read: 0.275,
                cache_write: 1.1,
            },
            context_window: 200_000,
            max_tokens: 100_000,
            headers: HashMap::new(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_returns_builtins() {
        let config = Config::default();
        let models = resolve_models(&config);
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().any(|m| m.id == "gpt-4.1"));
    }

    #[test]
    fn custom_provider_adds_models() {
        let mut config = Config::default();
        config.providers.insert(
            "my-qwen".into(),
            ProviderConfig {
                api: "openai".into(),
                base_url: "http://localhost:8080/v1".into(),
                api_key: None,
                models: vec![ModelConfig {
                    id: "qwen3.5-72b".into(),
                    name: Some("Qwen 3.5 72B".into()),
                    context_window: 131_072,
                    max_tokens: 32_768,
                    thinking: ThinkingStyle::Qwen,
                    cost: ModelCost::default(),
                }],
            },
        );
        let models = resolve_models(&config);
        let qwen = models.iter().find(|m| m.id == "qwen3.5-72b").unwrap();
        assert_eq!(qwen.provider, "my-qwen");
        assert_eq!(qwen.api, "openai-completions");
        assert_eq!(qwen.base_url, "http://localhost:8080/v1");
        assert_eq!(qwen.thinking, ThinkingStyle::Qwen);
    }

    #[test]
    fn config_overrides_builtin() {
        let mut config = Config::default();
        config.providers.insert(
            "openai".into(),
            ProviderConfig {
                api: "openai".into(),
                base_url: "http://my-proxy/v1".into(),
                api_key: None,
                models: vec![ModelConfig {
                    id: "gpt-4.1".into(),
                    name: Some("GPT-4.1 via proxy".into()),
                    context_window: 1_048_576,
                    max_tokens: 32_768,
                    thinking: ThinkingStyle::None,
                    cost: ModelCost::default(),
                }],
            },
        );
        let models = resolve_models(&config);
        let gpt = models
            .iter()
            .find(|m| m.id == "gpt-4.1" && m.provider == "openai")
            .unwrap();
        assert_eq!(gpt.base_url, "http://my-proxy/v1");
        assert_eq!(gpt.name, "GPT-4.1 via proxy");
    }

    #[test]
    fn env_expansion_in_api_key() {
        let pc = ProviderConfig {
            api: "openai".into(),
            base_url: "http://localhost".into(),
            api_key: Some("$HOME".into()),
            models: vec![],
        };
        let key = resolve_provider_api_key(&pc);
        assert!(key.is_some());

        let none_pc = ProviderConfig {
            api: "openai".into(),
            base_url: "http://localhost".into(),
            api_key: Some("none".into()),
            models: vec![],
        };
        assert!(resolve_provider_api_key(&none_pc).is_none());
    }

    #[test]
    fn roundtrip_toml() {
        let mut config = Config::default();
        config.providers.insert(
            "local".into(),
            ProviderConfig {
                api: "openai".into(),
                base_url: "http://localhost:8080/v1".into(),
                api_key: None,
                models: vec![ModelConfig {
                    id: "test-model".into(),
                    name: None,
                    context_window: 128_000,
                    max_tokens: 16_384,
                    thinking: ThinkingStyle::Qwen,
                    cost: ModelCost::default(),
                }],
            },
        );
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert!(parsed.providers.contains_key("local"));
        assert_eq!(parsed.providers["local"].models[0].id, "test-model");
    }
}
