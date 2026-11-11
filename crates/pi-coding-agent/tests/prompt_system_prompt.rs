//! Port of `system-prompt.test.ts`.

use pi_coding_agent::prompt::{BuildSystemPromptOptions, build_system_prompt};
use pi_coding_agent::skills::{Scope, Skill};

fn test_skill() -> Skill {
    Skill {
        name: "test-skill".to_string(),
        description: "A test skill.".to_string(),
        file_path: "/skills/test-skill/SKILL.md".into(),
        base_dir: "/skills/test-skill".into(),
        source: "test".to_string(),
        scope: Scope::Temporary,
        disable_model_invocation: false,
    }
}

fn build(options: &BuildSystemPromptOptions) -> String {
    build_system_prompt(options).expect("prompt builds")
}

#[test]
fn shows_none_for_empty_tools_list() {
    let prompt = build(&BuildSystemPromptOptions::new("/tmp"));
    assert!(prompt.contains("<tools>\n(none)\n"), "{prompt}");
}

#[test]
fn shows_file_paths_guideline_even_with_no_tools() {
    let prompt = build(&BuildSystemPromptOptions::new("/tmp"));
    assert!(prompt.contains("Show file paths clearly"));
}

#[test]
fn keeps_the_default_and_custom_prompt_prefixes_exact() {
    let default_prompt = build(&BuildSystemPromptOptions::new("/tmp"));
    assert!(
        default_prompt
            .starts_with("You are a coding assistant running inside pi, a coding agent harness."),
        "{}",
        &default_prompt[..80]
    );
    let custom = build(&BuildSystemPromptOptions {
        custom_prompt: Some("You are Exact.".to_string()),
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(
        custom.starts_with("You are Exact.\n\n<cwd>"),
        "{}",
        &custom[..80]
    );
}

#[test]
fn preserves_an_exact_forced_prompt_without_sections() {
    let prompt = build(&BuildSystemPromptOptions {
        force_system_prompt: Some("exact".to_string()),
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert_eq!(prompt, "exact");
}

#[test]
fn maps_appended_instructions_and_project_context_to_stable_sections() {
    let prompt = build(&BuildSystemPromptOptions {
        custom_prompt: Some("You are Exact.".to_string()),
        append_system_prompt: Some("Additional instructions.".to_string()),
        context_files: vec![pi_coding_agent::prompt::ContextFile {
            path: "/tmp/AGENTS.md".to_string(),
            content: "Project instructions.".to_string(),
        }],
        selected_tools: Some(Vec::new()),
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(
        prompt.contains("<addendum>\nAdditional instructions.\n</addendum>"),
        "{prompt}"
    );
    assert!(
        prompt.contains(
            "<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"/tmp/AGENTS.md\">"
        ),
        "{prompt}"
    );
    assert!(prompt.contains("<cwd>\n/tmp\n</cwd>"));
}

#[test]
fn includes_all_default_tools_when_snippets_are_provided() {
    let prompt = build(&BuildSystemPromptOptions {
        tool_snippets: [
            ("read".to_string(), "Read file contents".to_string()),
            ("bash".to_string(), "Execute bash commands".to_string()),
            ("edit".to_string(), "Make surgical edits".to_string()),
            ("write".to_string(), "Create or overwrite files".to_string()),
        ]
        .into_iter()
        .collect(),
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(prompt.contains("- read:"));
    assert!(prompt.contains("- bash:"));
    assert!(prompt.contains("- edit:"));
    assert!(prompt.contains("- write:"));
}

#[test]
fn uses_shell_specific_guidance() {
    let prompt = build(&BuildSystemPromptOptions {
        selected_tools: Some(vec!["powershell".to_string()]),
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(
        prompt.contains("Use PowerShell for file operations"),
        "{prompt}"
    );

    let prompt = build(&BuildSystemPromptOptions {
        selected_tools: Some(vec!["bash".to_string(), "powershell".to_string()]),
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(
        prompt.contains("Use bash or PowerShell for file operations"),
        "{prompt}"
    );
}

#[test]
fn instructs_models_to_resolve_pi_docs_under_absolute_base_paths() {
    let prompt = build(&BuildSystemPromptOptions::new("/tmp"));
    assert!(
        prompt.contains(
            "- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory"
        ),
        "{prompt}"
    );
    assert!(prompt.contains(
        "environment variables (docs/environment-variables.md), MCP servers (docs/mcp.md)"
    ));
    assert!(prompt.contains(
        "Before implementing harness changes, read the relevant docs and examples in full, following related cross-references."
    ));
}

#[test]
fn includes_custom_tools_only_when_snippets_are_provided() {
    let prompt = build(&BuildSystemPromptOptions {
        selected_tools: Some(vec!["read".to_string(), "dynamic_tool".to_string()]),
        tool_snippets: [(
            "dynamic_tool".to_string(),
            "Run dynamic test behavior".to_string(),
        )]
        .into_iter()
        .collect(),
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(
        prompt.contains("- dynamic_tool: Run dynamic test behavior"),
        "{prompt}"
    );

    let prompt = build(&BuildSystemPromptOptions {
        selected_tools: Some(vec!["read".to_string(), "dynamic_tool".to_string()]),
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(!prompt.contains("dynamic_tool"), "{prompt}");
}

#[test]
fn appends_and_deduplicates_prompt_guidelines() {
    let prompt = build(&BuildSystemPromptOptions {
        selected_tools: Some(vec!["read".to_string(), "dynamic_tool".to_string()]),
        prompt_guidelines: vec!["Use dynamic_tool for project summaries.".to_string()],
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(prompt.contains("- Use dynamic_tool for project summaries."));

    let prompt = build(&BuildSystemPromptOptions {
        selected_tools: Some(vec!["read".to_string(), "dynamic_tool".to_string()]),
        prompt_guidelines: vec![
            "Use dynamic_tool for summaries.".to_string(),
            "  Use dynamic_tool for summaries.  ".to_string(),
            "   ".to_string(),
        ],
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert_eq!(
        prompt.matches("- Use dynamic_tool for summaries.").count(),
        1,
        "{prompt}"
    );
}

#[test]
fn includes_skills_with_only_bash_in_default_and_custom_prompts() {
    for custom_prompt in [None, Some("Custom system prompt".to_string())] {
        let prompt = build(&BuildSystemPromptOptions {
            custom_prompt,
            selected_tools: Some(vec!["bash".to_string()]),
            skills: vec![test_skill()],
            ..BuildSystemPromptOptions::new("/tmp")
        });
        assert!(prompt.contains("<skills>"), "{prompt}");
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>test-skill</name>"));
        assert!(prompt.contains("Use bash to load a skill's file"));
        assert!(prompt.contains(
            "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls."
        ));
    }
}

#[test]
fn omits_skills_without_read_or_bash() {
    let prompt = build(&BuildSystemPromptOptions {
        selected_tools: Some(vec!["write".to_string()]),
        skills: vec![test_skill()],
        ..BuildSystemPromptOptions::new("/tmp")
    });
    assert!(!prompt.contains("<available_skills>"), "{prompt}");
}

// #10343 — hidden tools
fn build_with_hidden(hidden_tools: Vec<String>) -> String {
    build(&BuildSystemPromptOptions {
        selected_tools: Some(vec![
            "read".to_string(),
            "bash".to_string(),
            "run".to_string(),
        ]),
        hidden_tools,
        tool_snippets: [
            ("read".to_string(), "Read files".to_string()),
            ("bash".to_string(), "Run commands".to_string()),
            ("run".to_string(), "Run a task".to_string()),
        ]
        .into_iter()
        .collect(),
        tool_guidelines: [
            ("read".to_string(), vec!["Use read for files.".to_string()]),
            ("run".to_string(), vec!["Prefer run.".to_string()]),
        ]
        .into_iter()
        .collect(),
        skills: vec![test_skill()],
        ..BuildSystemPromptOptions::new("/tmp")
    })
}

#[test]
fn leaves_hidden_tools_out_of_the_tool_list_and_rules() {
    let prompt = build_with_hidden(vec!["read".to_string(), "bash".to_string()]);
    assert!(prompt.contains("<tools>\n- run: Run a task\n"), "{prompt}");
    assert!(!prompt.contains("- read: "));
    assert!(!prompt.contains("Use read for files."));
    assert!(!prompt.contains("Use bash for file operations"));
    assert!(prompt.contains("- Prefer run."));
}

#[test]
fn keeps_skills_without_naming_a_hidden_reader() {
    assert!(
        build_with_hidden(vec!["read".to_string(), "bash".to_string()])
            .contains("\nLoad a skill's file when the task matches its description.")
    );
    assert!(
        build_with_hidden(vec!["read".to_string()]).contains("Use bash to load a skill's file")
    );
    assert!(build_with_hidden(Vec::new()).contains("Use the read tool to load a skill's file"));
}

#[test]
fn rejects_invalid_custom_section_names() {
    let error = build_system_prompt(&BuildSystemPromptOptions {
        sections: vec![("Preamble".to_string(), "x".to_string())],
        ..BuildSystemPromptOptions::new("/tmp")
    })
    .unwrap_err();
    assert_eq!(error, "Invalid system prompt section name: Preamble");
    let error = build_system_prompt(&BuildSystemPromptOptions {
        sections: vec![("preamble".to_string(), "x".to_string())],
        ..BuildSystemPromptOptions::new("/tmp")
    })
    .unwrap_err();
    assert_eq!(error, "Invalid system prompt section name: preamble");
}
