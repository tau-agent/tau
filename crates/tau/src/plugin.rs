//! Subprocess plugin system.
//!
//! Plugins are external processes that communicate via JSON-lines on stdin/stdout.
//! They can register tools, hooks, and slash commands.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::types::{Tool, ToolCall, ToolResultContent};

// ---------------------------------------------------------------------------
// Protocol messages: tau → plugin
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginRequest {
    /// Initialize the plugin with session context.
    Init { cwd: String, session_id: String },
    /// Call a hook.
    Hook {
        name: String,
        data: serde_json::Value,
    },
    /// Execute a tool call.
    ToolCall {
        tool_call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// Notify session start.
    SessionStart { cwd: String, session_id: String },
}

// ---------------------------------------------------------------------------
// Protocol messages: plugin → tau
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    /// Plugin registration (sent once on startup).
    Register(PluginRegistration),
    /// Hook result.
    HookResult(HookResult),
    /// Tool execution result (final).
    ToolResult(PluginToolResult),
    /// Tool output delta (streaming).
    OutputDelta { tool_call_id: String, text: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginRegistration {
    /// Plugin name.
    pub name: String,
    /// Tools provided by this plugin.
    #[serde(default)]
    pub tools: Vec<PluginToolDef>,
    /// Hooks this plugin wants to receive.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Slash commands provided by this plugin.
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginToolDef {
    /// Tool name.
    pub name: String,
    /// Tool description (for LLM).
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: serde_json::Value,
    /// One-line snippet for system prompt "Available tools:" list.
    #[serde(default)]
    pub prompt_snippet: Option<String>,
    /// Extra guideline bullets for system prompt.
    #[serde(default)]
    pub prompt_guidelines: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginCommand {
    /// Command name (without /).
    pub name: String,
    /// Description shown in /help.
    pub description: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct HookResult {
    /// Optional message to inject before the LLM turn.
    #[serde(default)]
    pub message: Option<HookMessage>,
    /// Optional replacement system prompt.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Optional text to append to a tool result (for after_tool_result hook).
    #[serde(default)]
    pub tool_result_append: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookMessage {
    /// Content of the injected message.
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct PluginToolResult {
    pub tool_call_id: String,
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Plugin handle
// ---------------------------------------------------------------------------

/// A running plugin process.
pub struct PluginHandle {
    pub name: String,
    pub registration: PluginRegistration,
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl PluginHandle {
    /// Spawn a plugin process and read its registration.
    pub fn spawn(command: &[String], cwd: &str) -> crate::Result<Self> {
        if command.is_empty() {
            return Err(crate::Error::Io("empty plugin command".into()));
        }

        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| crate::Error::Io(format!("spawn plugin {:?}: {}", command, e)))?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| crate::Error::Io("plugin stdin not available".into()))?,
        );
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| crate::Error::Io("plugin stdout not available".into()))?,
        );

        let mut handle = Self {
            name: String::new(),
            registration: PluginRegistration {
                name: String::new(),
                tools: Vec::new(),
                hooks: Vec::new(),
                commands: Vec::new(),
            },
            child,
            stdin,
            stdout,
        };

        // Read the registration message
        let msg = handle.read_message()?;
        match msg {
            PluginMessage::Register(reg) => {
                handle.name = reg.name.clone();
                handle.registration = reg;
            }
            _ => {
                return Err(crate::Error::Io(
                    "plugin first message must be Register".into(),
                ));
            }
        }

        Ok(handle)
    }

    /// Send a request to the plugin.
    pub fn send(&mut self, req: &PluginRequest) -> crate::Result<()> {
        let mut line =
            serde_json::to_string(req).map_err(|e| crate::Error::Parse(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| crate::Error::Io(format!("write to plugin {}: {}", self.name, e)))?;
        self.stdin
            .flush()
            .map_err(|e| crate::Error::Io(format!("flush plugin {}: {}", self.name, e)))?;
        Ok(())
    }

    /// Read a single message from the plugin.
    pub fn read_message(&mut self) -> crate::Result<PluginMessage> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|e| crate::Error::Io(format!("read from plugin {}: {}", self.name, e)))?;
        if line.is_empty() {
            return Err(crate::Error::Io(format!(
                "plugin {} closed unexpectedly",
                self.name
            )));
        }
        serde_json::from_str(&line)
            .map_err(|e| crate::Error::Parse(format!("plugin {} message: {}", self.name, e)))
    }

    /// Execute a tool call, calling on_output for streaming deltas.
    pub fn execute_tool(
        &mut self,
        tool_call: &ToolCall,
        on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<crate::types::ToolResultMessage> {
        self.send(&PluginRequest::ToolCall {
            tool_call_id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
        })?;

        loop {
            let msg = self.read_message()?;
            match msg {
                PluginMessage::OutputDelta { text, .. } => {
                    on_output(&text);
                }
                PluginMessage::ToolResult(result) => {
                    return Ok(crate::types::ToolResultMessage {
                        tool_call_id: result.tool_call_id,
                        tool_name: tool_call.name.clone(),
                        content: result.content,
                        details: None,
                        is_error: result.is_error,
                        timestamp: crate::types::timestamp_ms(),
                    });
                }
                _ => {
                    // Ignore unexpected messages during tool execution
                }
            }
        }
    }

    /// Call a hook on this plugin.
    pub fn call_hook(&mut self, name: &str, data: serde_json::Value) -> crate::Result<HookResult> {
        self.send(&PluginRequest::Hook {
            name: name.to_string(),
            data,
        })?;

        let msg = self.read_message()?;
        match msg {
            PluginMessage::HookResult(result) => Ok(result),
            _ => Ok(HookResult::default()),
        }
    }

    /// Kill the plugin process.
    pub fn kill(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// Check if plugin wants a specific hook.
    pub fn wants_hook(&self, name: &str) -> bool {
        self.registration.hooks.iter().any(|h| h == name)
    }

    /// Get Tool definitions for LLM.
    pub fn tool_schemas(&self) -> Vec<Tool> {
        self.registration
            .tools
            .iter()
            .map(|t| Tool {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            })
            .collect()
    }

    /// Get tool prompt contributions.
    pub fn tool_prompts(&self) -> Vec<crate::system_prompt::ToolPrompt> {
        self.registration
            .tools
            .iter()
            .filter_map(|t| {
                t.prompt_snippet
                    .as_ref()
                    .map(|snippet| crate::system_prompt::ToolPrompt {
                        name: t.name.clone(),
                        snippet: snippet.clone(),
                        guidelines: t.prompt_guidelines.clone(),
                    })
            })
            .collect()
    }
}

impl Drop for PluginHandle {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// Plugin manager
// ---------------------------------------------------------------------------

/// Manages all loaded plugins.
pub struct PluginManager {
    plugins: Vec<PluginHandle>,
    /// Sessions that have already received session_start.
    initialized_sessions: std::collections::HashSet<String>,
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginManager {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            initialized_sessions: std::collections::HashSet::new(),
        }
    }

    /// Load plugins from config.
    pub fn load_from_config(config: &PluginsConfig, cwd: &str) -> Self {
        let mut manager = Self::new();
        for (name, plugin_config) in &config.plugins {
            match PluginHandle::spawn(&plugin_config.command, cwd) {
                Ok(handle) => {
                    let tools: Vec<&str> = handle
                        .registration
                        .tools
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect();
                    let hooks = &handle.registration.hooks;
                    eprintln!(
                        "plugin '{}': {} tools {:?}, {} hooks {:?}",
                        handle.name,
                        tools.len(),
                        tools,
                        hooks.len(),
                        hooks,
                    );
                    manager.plugins.push(handle);
                }
                Err(e) => {
                    eprintln!("plugin '{}' failed to load: {}", name, e);
                }
            }
        }
        manager
    }

    /// Get all tool schemas from all plugins.
    pub fn tool_schemas(&self) -> Vec<Tool> {
        self.plugins.iter().flat_map(|p| p.tool_schemas()).collect()
    }

    /// Get all tool prompt contributions from all plugins.
    pub fn tool_prompts(&self) -> Vec<crate::system_prompt::ToolPrompt> {
        self.plugins.iter().flat_map(|p| p.tool_prompts()).collect()
    }

    /// Find which plugin handles a tool by name.
    pub fn find_tool_plugin(&mut self, tool_name: &str) -> Option<&mut PluginHandle> {
        self.plugins
            .iter_mut()
            .find(|p| p.registration.tools.iter().any(|t| t.name == tool_name))
    }

    /// Call a hook on all plugins that want it. Returns merged results.
    pub fn call_hook(&mut self, name: &str, data: &serde_json::Value) -> Vec<HookResult> {
        let mut results = Vec::new();
        for plugin in &mut self.plugins {
            if plugin.wants_hook(name) {
                match plugin.call_hook(name, data.clone()) {
                    Ok(result) => results.push(result),
                    Err(e) => eprintln!("plugin {} hook {} error: {}", plugin.name, name, e),
                }
            }
        }
        results
    }

    /// Notify session start to all plugins (only once per session).
    pub fn notify_session_start_once(&mut self, cwd: &str, session_id: &str) {
        if !self.initialized_sessions.insert(session_id.to_string()) {
            return; // already notified
        }
        for plugin in &mut self.plugins {
            if plugin.wants_hook("session_start") {
                let _ = plugin.send(&PluginRequest::SessionStart {
                    cwd: cwd.to_string(),
                    session_id: session_id.to_string(),
                });
                // Read and discard the hook result
                let _ = plugin.read_message();
            }
        }
    }

    /// Get all slash commands from plugins.
    pub fn commands(&self) -> Vec<(String, String)> {
        self.plugins
            .iter()
            .flat_map(|p| {
                p.registration
                    .commands
                    .iter()
                    .map(|c| (c.name.clone(), c.description.clone()))
            })
            .collect()
    }

    /// Kill all plugins.
    pub fn kill_all(&mut self) {
        for plugin in &mut self.plugins {
            plugin.kill();
        }
    }

    /// Check if any plugin provides a given tool.
    pub fn has_tool(&self, name: &str) -> bool {
        self.plugins
            .iter()
            .any(|p| p.registration.tools.iter().any(|t| t.name == name))
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        self.kill_all();
    }
}

// ---------------------------------------------------------------------------
// Plugin config
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PluginsConfig {
    #[serde(default)]
    pub plugins: HashMap<String, PluginEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Command to spawn the plugin (e.g. ["node", "/path/to/plugin.js"]).
    pub command: Vec<String>,
}

/// Load plugins config from `~/.config/tau/plugins.toml`.
pub fn load_plugins_config() -> PluginsConfig {
    let path = if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(config)
            .join("tau")
            .join("plugins.toml")
    } else if let Ok(home) = std::env::var("HOME") {
        std::path::PathBuf::from(home)
            .join(".config")
            .join("tau")
            .join("plugins.toml")
    } else {
        return PluginsConfig::default();
    };

    if !path.exists() {
        return PluginsConfig::default();
    }

    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| toml::from_str(&content).ok())
        .unwrap_or_default()
}
