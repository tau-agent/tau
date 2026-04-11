//! Tool prompt contribution type.

/// A tool's contribution to the system prompt.
#[derive(Debug, Clone)]
pub struct ToolPrompt {
    /// Tool name (e.g. "bash").
    pub name: String,
    /// One-line description for the "Available tools" list.
    pub snippet: String,
    /// Extra guideline bullets for the "Guidelines" section.
    pub guidelines: Vec<String>,
}
