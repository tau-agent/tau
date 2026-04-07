//! System prompt construction for the coding agent.
//!
//! Dynamic: tools contribute their own snippets and guidelines.

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

/// Options for building the system prompt.
#[derive(Debug, Default)]
pub struct PromptOptions {
    /// Working directory.
    pub cwd: Option<String>,
    /// Tool prompt contributions.
    pub tools: Vec<ToolPrompt>,
    /// Extra guidelines (from config, extensions, etc.).
    pub extra_guidelines: Vec<String>,
    /// Text appended after the main prompt.
    pub append: Option<String>,
}

/// Build the system prompt.
pub fn build(options: &PromptOptions) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Tools list
    let tools_list = if options.tools.is_empty() {
        "(none)".to_string()
    } else {
        options
            .tools
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Guidelines: collect from tools, then extras, then always-on
    let mut guidelines: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |g: String| {
        if seen.insert(g.clone()) {
            guidelines.push(g);
        }
    };

    for tool in &options.tools {
        for g in &tool.guidelines {
            add(g.clone());
        }
    }
    for g in &options.extra_guidelines {
        add(g.clone());
    }

    // Always-on guidelines
    add("Be concise in your responses".into());
    add("Show file paths clearly when working with files".into());

    let guidelines_str = guidelines
        .iter()
        .map(|g| format!("- {}", g))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        r#"You are an expert coding assistant operating inside tau, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
{tools_list}

In addition to the tools above, you may have access to other custom tools depending on the project.

Guidelines:
{guidelines_str}
Current date: {date}"#
    );

    if let Some(cwd) = &options.cwd {
        prompt.push_str(&format!("\nCurrent working directory: {}", cwd));
    }

    if let Some(append) = &options.append {
        prompt.push_str("\n\n");
        prompt.push_str(append);
    }

    prompt
}

/// Built-in tool prompts for the default tools.
pub fn default_tool_prompts() -> Vec<ToolPrompt> {
    vec![
        ToolPrompt {
            name: "bash".into(),
            snippet: "Execute bash commands (ls, grep, find, etc.)".into(),
            guidelines: vec!["Use bash for file operations like ls, rg, find".into()],
        },
        ToolPrompt {
            name: "read".into(),
            snippet: "Read file contents".into(),
            guidelines: vec!["Use read to examine files instead of cat or sed.".into()],
        },
        ToolPrompt {
            name: "edit".into(),
            snippet: "Make precise file edits with exact text replacement, including multiple disjoint edits in one call".into(),
            guidelines: vec![
                "Use edit for precise changes (old text must match exactly)".into(),
                "When changing multiple separate locations in one file, use one edit call with edits[] instead of multiple edit calls".into(),
                "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.".into(),
                "Keep oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.".into(),
            ],
        },
        ToolPrompt {
            name: "write".into(),
            snippet: "Create or overwrite files".into(),
            guidelines: vec!["Use write only for new files or complete rewrites.".into()],
        },
    ]
}

/// Build a system prompt with the default tools (convenience for server).
pub fn build_default(cwd: Option<&str>) -> String {
    build(&PromptOptions {
        cwd: cwd.map(String::from),
        tools: default_tool_prompts(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_contains_tools() {
        let prompt = build_default(Some("/tmp"));
        assert!(prompt.contains("Available tools:"));
        assert!(prompt.contains("- bash:"));
        assert!(prompt.contains("- read:"));
        assert!(prompt.contains("- edit:"));
        assert!(prompt.contains("- write:"));
    }

    #[test]
    fn default_prompt_contains_guidelines() {
        let prompt = build_default(None);
        assert!(prompt.contains("Use bash for file operations"));
        assert!(prompt.contains("Use read to examine files"));
        assert!(prompt.contains("Use edit for precise changes"));
        assert!(prompt.contains("Use write only for new files"));
        assert!(prompt.contains("Be concise"));
    }

    #[test]
    fn prompt_has_identity() {
        let prompt = build_default(None);
        assert!(prompt.contains("operating inside tau"));
    }

    #[test]
    fn prompt_has_date() {
        let prompt = build_default(None);
        assert!(prompt.contains("Current date:"));
    }

    #[test]
    fn prompt_has_cwd() {
        let prompt = build_default(Some("/home/user/project"));
        assert!(prompt.contains("Current working directory: /home/user/project"));
    }

    #[test]
    fn prompt_without_cwd() {
        let prompt = build_default(None);
        assert!(!prompt.contains("Current working directory:"));
    }

    #[test]
    fn custom_tools_and_guidelines() {
        let prompt = build(&PromptOptions {
            tools: vec![ToolPrompt {
                name: "deploy".into(),
                snippet: "Deploy to production".into(),
                guidelines: vec!["Always confirm before deploying".into()],
            }],
            extra_guidelines: vec!["Use cargo fmt before committing".into()],
            ..Default::default()
        });
        assert!(prompt.contains("- deploy: Deploy to production"));
        assert!(prompt.contains("Always confirm before deploying"));
        assert!(prompt.contains("Use cargo fmt before committing"));
    }

    #[test]
    fn no_sudo_line() {
        let prompt = build_default(None);
        assert!(!prompt.contains("sudo"));
    }

    #[test]
    fn deduplicates_guidelines() {
        let prompt = build(&PromptOptions {
            tools: default_tool_prompts(),
            extra_guidelines: vec!["Be concise in your responses".into()],
            ..Default::default()
        });
        // Should only appear once
        let count = prompt.matches("Be concise in your responses").count();
        assert_eq!(count, 1);
    }
}
