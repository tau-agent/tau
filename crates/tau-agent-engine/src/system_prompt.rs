//! System prompt construction for the coding agent.
//!
//! Dynamic: tools contribute their own snippets and guidelines.

// Re-export ToolPrompt from tau-agent-base for backward compatibility
pub use tau_agent_base::tool_prompt::ToolPrompt;

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

    // Guidelines: group by tool, then a General section for extras + always-on.
    // Dedup globally so the same line never appears twice.
    let mut seen = std::collections::HashSet::new();

    // Per-tool sections (order matches options.tools).
    let mut tool_sections: Vec<String> = Vec::new();
    for tool in &options.tools {
        let mut bullets: Vec<String> = Vec::new();
        for g in &tool.guidelines {
            if seen.insert(g.clone()) {
                bullets.push(format!("- {}", g));
            }
        }
        if !bullets.is_empty() {
            let mut section = format!("{}:\n", tool.name);
            section.push_str(&bullets.join("\n"));
            tool_sections.push(section);
        }
    }

    // General section: extras + always-on, minus anything already placed under a tool.
    let mut general_bullets: Vec<String> = Vec::new();
    let add_general = |g: String,
                       seen: &mut std::collections::HashSet<String>,
                       general_bullets: &mut Vec<String>| {
        if seen.insert(g.clone()) {
            general_bullets.push(format!("- {}", g));
        }
    };
    for g in &options.extra_guidelines {
        add_general(g.clone(), &mut seen, &mut general_bullets);
    }
    add_general(
        "Be concise in your responses".into(),
        &mut seen,
        &mut general_bullets,
    );
    add_general(
        "When referring to files, use paths relative to the current working directory (e.g. crates/foo/src/lib.rs). Use absolute paths only when the file is outside the project.".into(),
        &mut seen,
        &mut general_bullets,
    );

    let mut sections: Vec<String> = Vec::new();
    if !general_bullets.is_empty() {
        sections.push(general_bullets.join("\n"));
    }
    sections.extend(tool_sections);
    let guidelines_str = sections.join("\n\n");

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
        prompt.push_str(&format!(
            "\nCurrent working directory: {cwd}\n\
             Bash commands already run in this directory — do not prefix them with `cd {cwd} && ...`.",
        ));
    }

    if let Some(append) = &options.append {
        prompt.push_str("\n\n");
        prompt.push_str(append);
    }

    prompt
}

/// Built-in tool prompts for the default tools.
///
/// Delegates to the canonical source in `tau_agent_plugin::default_tool_prompts()`.
pub fn default_tool_prompts() -> Vec<ToolPrompt> {
    tau_agent_plugin::default_tool_prompts()
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
    fn default_prompt_mentions_multi_file_edit_batching() {
        // Regression guard: future edits to the prompt must keep the
        // multi-file batching guidance, otherwise the model will silently
        // revert to one edit-call per file.
        let prompt = build_default(None);
        assert!(
            prompt.contains("files: [{path, edits: [...]}"),
            "expected multi-file edit guidance in prompt:\n{prompt}"
        );
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
    fn prompt_cwd_includes_no_cd_nudge() {
        let prompt = build_default(Some("/tmp"));
        assert!(
            prompt.contains("already run in this directory"),
            "expected cwd nudge in prompt:\n{prompt}"
        );
        assert!(
            prompt.contains("do not prefix them with `cd /tmp && ...`"),
            "expected concrete cd example in prompt:\n{prompt}"
        );
    }

    #[test]
    fn prompt_without_cwd_has_no_nudge() {
        let prompt = build_default(None);
        assert!(!prompt.contains("Current working directory"));
        assert!(!prompt.contains("already run in this directory"));
        assert!(!prompt.contains("do not prefix them with `cd"));
    }

    #[test]
    fn file_path_guideline_is_concrete() {
        let prompt = build_default(None);
        assert!(
            prompt.contains(
                "When referring to files, use paths relative to the current working directory"
            ),
            "expected concrete file-path guideline:\n{prompt}"
        );
        assert!(
            !prompt.contains("Show file paths clearly when working with files"),
            "old vague guideline should be gone:\n{prompt}"
        );
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

    #[test]
    fn grouped_guidelines_section_per_tool() {
        let prompt = build(&PromptOptions {
            tools: vec![
                ToolPrompt {
                    name: "toolA".into(),
                    snippet: "A".into(),
                    guidelines: vec!["guidance A".into()],
                },
                ToolPrompt {
                    name: "toolB".into(),
                    snippet: "B".into(),
                    guidelines: vec!["guidance B".into()],
                },
            ],
            ..Default::default()
        });
        let guidelines = prompt
            .split("Guidelines:")
            .nth(1)
            .expect("Guidelines section");
        assert!(
            guidelines.contains("toolA:\n- guidance A"),
            "guidelines section:\n{guidelines}"
        );
        assert!(
            guidelines.contains("toolB:\n- guidance B"),
            "guidelines section:\n{guidelines}"
        );
        // toolA section appears before toolB section within the guidelines.
        let idx_a = guidelines.find("toolA:").expect("toolA heading");
        let idx_b = guidelines.find("toolB:").expect("toolB heading");
        assert!(idx_a < idx_b);
    }

    #[test]
    fn grouped_guidelines_empty_tool_has_no_heading() {
        let prompt = build(&PromptOptions {
            tools: vec![ToolPrompt {
                name: "silent".into(),
                snippet: "no guidance".into(),
                guidelines: vec![],
            }],
            ..Default::default()
        });
        // Tool still appears in the "Available tools:" list...
        assert!(prompt.contains("- silent: no guidance"));
        // ...but there should be no "silent:" heading in the Guidelines section.
        let guidelines_section = prompt
            .split("Guidelines:")
            .nth(1)
            .expect("Guidelines section");
        assert!(
            !guidelines_section.contains("silent:"),
            "guidelines section unexpectedly contained 'silent:' heading:\n{guidelines_section}"
        );
    }

    #[test]
    fn extra_guidelines_grouped_under_general() {
        let prompt = build(&PromptOptions {
            tools: vec![ToolPrompt {
                name: "toolA".into(),
                snippet: "A".into(),
                guidelines: vec!["guidance A".into()],
            }],
            extra_guidelines: vec!["Use cargo fmt before committing".into()],
            ..Default::default()
        });
        let guidelines = prompt
            .split("Guidelines:")
            .nth(1)
            .expect("Guidelines section");
        let idx_extra = guidelines
            .find("Use cargo fmt before committing")
            .expect("extra guideline present");
        let idx_tool = guidelines.find("toolA:").expect("toolA heading");
        assert!(
            idx_extra < idx_tool,
            "expected extra guideline to appear before tool section; guidelines:\n{guidelines}"
        );
    }

    #[test]
    fn always_on_guidelines_always_present() {
        let prompt = build(&PromptOptions {
            tools: vec![ToolPrompt {
                name: "toolA".into(),
                snippet: "A".into(),
                guidelines: vec!["guidance A".into()],
            }],
            ..Default::default()
        });
        assert_eq!(prompt.matches("Be concise in your responses").count(), 1);
        assert_eq!(
            prompt
                .matches(
                    "When referring to files, use paths relative to the current working directory"
                )
                .count(),
            1
        );
    }

    #[test]
    fn tool_section_wins_dedup_over_general() {
        // A guideline appearing both under a tool and in extra_guidelines is
        // emitted exactly once, under the tool heading.
        let shared = "Shared guideline";
        let prompt = build(&PromptOptions {
            tools: vec![ToolPrompt {
                name: "toolA".into(),
                snippet: "A".into(),
                guidelines: vec![shared.into()],
            }],
            extra_guidelines: vec![shared.into()],
            ..Default::default()
        });
        assert_eq!(prompt.matches(shared).count(), 1);
        // It should appear under the toolA heading (in the Guidelines section).
        let guidelines = prompt
            .split("Guidelines:")
            .nth(1)
            .expect("Guidelines section");
        let after_heading = guidelines
            .split("toolA:")
            .nth(1)
            .expect("toolA section present");
        assert!(after_heading.contains(shared));
    }
}
