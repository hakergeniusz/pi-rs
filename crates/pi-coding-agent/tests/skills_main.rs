//! Port of `skills.test.ts`, driven by the fixture directories copied from
//! the TS test suite (`tests/fixtures/skills`, `tests/fixtures/skills-collision`).

use std::path::PathBuf;

use pi_coding_agent::skills::{
    DiagnosticKind, FileReadTool, LoadSkillsFromDirOptions, LoadSkillsOptions, ResourceDiagnostic,
    Scope, Skill, format_skills_for_prompt, load_skills, load_skills_from_dir,
};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills")
}

fn collision_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills-collision")
}

fn create_test_skill(
    name: &str,
    description: &str,
    file_path: &str,
    base_dir: &str,
    disable_model_invocation: bool,
    source: &str,
) -> Skill {
    Skill {
        name: name.to_string(),
        description: description.to_string(),
        file_path: file_path.into(),
        base_dir: base_dir.into(),
        source: source.to_string(),
        scope: Scope::Temporary,
        disable_model_invocation,
    }
}

fn warnings(result: &pi_coding_agent::skills::LoadSkillsResult) -> Vec<&ResourceDiagnostic> {
    result
        .diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic.kind, DiagnosticKind::Warning))
        .collect()
}

// --- loadSkillsFromDir -------------------------------------------------------

#[test]
fn loads_a_valid_skill() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("valid-skill"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "valid-skill");
    assert_eq!(
        result.skills[0].description,
        "A valid skill for testing purposes."
    );
    assert_eq!(result.skills[0].source, "test");
    assert_eq!(warnings(&result).len(), 0);
}

#[test]
fn allows_names_that_do_not_match_parent_directory() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("name-mismatch"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "different-name");
    assert!(
        !warnings(&result).iter().any(|diagnostic| diagnostic
            .message
            .contains("does not match parent directory")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn warns_when_name_contains_invalid_characters() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("invalid-name-chars"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert!(
        warnings(&result)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("invalid characters")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn warns_when_name_exceeds_64_characters() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("long-name"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert!(
        warnings(&result)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("exceeds 64 characters")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn warns_and_skips_skill_when_description_is_missing() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("missing-description"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 0);
    assert!(
        warnings(&result)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("description is required")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn ignores_unknown_frontmatter_fields() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("unknown-field"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(warnings(&result).len(), 0);
}

#[test]
fn loads_nested_skills_recursively() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("nested"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "child-skill");
    assert_eq!(warnings(&result).len(), 0);
}

#[test]
fn prefers_a_directorys_root_skill_md_over_nested_ones() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("root-skill-preferred"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "root-skill-preferred");
    assert_eq!(result.skills[0].description, "Root skill should win.");
    assert_eq!(warnings(&result).len(), 0);
}

#[test]
fn skips_files_without_frontmatter() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("no-frontmatter"),
        source: "test".to_string(),
    });
    // no-frontmatter has no description, so it should be skipped.
    assert_eq!(result.skills.len(), 0);
    assert!(
        warnings(&result)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("description is required")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn warns_and_skips_skill_when_yaml_frontmatter_is_invalid() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("invalid-yaml"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 0);
    assert!(
        warnings(&result)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("at line")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn preserves_multiline_descriptions_from_yaml() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("multiline-description"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert!(
        result.skills[0].description.contains('\n'),
        "{:?}",
        result.skills[0].description
    );
    assert!(
        result.skills[0]
            .description
            .contains("This is a multiline description.")
    );
    assert_eq!(warnings(&result).len(), 0);
}

#[test]
fn warns_when_name_contains_consecutive_hyphens() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("consecutive-hyphens"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert!(
        warnings(&result)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("consecutive hyphens")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn loads_all_skills_from_the_fixture_directory() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir(),
        source: "test".to_string(),
    });
    // Should load all skills that have descriptions (even with warnings):
    // valid-skill, name-mismatch, invalid-name-chars, long-name, unknown-field,
    // nested/child-skill, consecutive-hyphens. NOT: missing-description,
    // no-frontmatter (both missing descriptions).
    assert!(
        result.skills.len() >= 6,
        "{:?}",
        result
            .skills
            .iter()
            .map(|skill| &skill.name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn returns_empty_for_non_existent_directory() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: "/non/existent/path".into(),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 0);
    assert_eq!(result.diagnostics.len(), 0);
}

#[test]
fn uses_parent_directory_name_when_name_not_in_frontmatter() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("valid-skill"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "valid-skill");
}

#[test]
fn parses_disable_model_invocation_frontmatter_field() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("disable-model-invocation"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "disable-model-invocation");
    assert!(result.skills[0].disable_model_invocation);
    // Should not warn about unknown field.
    assert!(
        !warnings(&result)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown frontmatter field")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn defaults_disable_model_invocation_to_false() {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: fixtures_dir().join("valid-skill"),
        source: "test".to_string(),
    });
    assert_eq!(result.skills.len(), 1);
    assert!(!result.skills[0].disable_model_invocation);
}

// --- formatSkillsForPrompt ---------------------------------------------------

#[test]
fn returns_empty_string_for_no_skills() {
    assert_eq!(format_skills_for_prompt(&[], FileReadTool::Read), "");
}

#[test]
fn formats_skills_as_xml() {
    let skills = vec![create_test_skill(
        "test-skill",
        "A test skill.",
        "/path/to/skill/SKILL.md",
        "/path/to/skill",
        false,
        "test",
    )];
    let result = format_skills_for_prompt(&skills, FileReadTool::Read);
    assert!(result.contains("<available_skills>"));
    assert!(result.contains("</available_skills>"));
    assert!(result.contains("<skill>"));
    assert!(result.contains("<name>test-skill</name>"));
    assert!(result.contains("<description>A test skill.</description>"));
    assert!(result.contains("<location>/path/to/skill/SKILL.md</location>"));
}

#[test]
fn includes_intro_text_before_xml() {
    let skills = vec![create_test_skill(
        "test-skill",
        "A test skill.",
        "/path/to/skill/SKILL.md",
        "/path/to/skill",
        false,
        "test",
    )];
    let result = format_skills_for_prompt(&skills, FileReadTool::Read);
    let xml_start = result.find("<available_skills>").expect("xml");
    let intro_text = &result[..xml_start];
    assert!(intro_text.contains("The following skills provide specialized instructions"));
    assert!(intro_text.contains("Use the read tool to load a skill's file"));
}

#[test]
fn escapes_xml_special_characters() {
    let skills = vec![create_test_skill(
        "test-skill",
        "A skill with <special> & \"characters\".",
        "/path/to/skill/SKILL.md",
        "/path/to/skill",
        false,
        "test",
    )];
    let result = format_skills_for_prompt(&skills, FileReadTool::Read);
    assert!(result.contains("&lt;special&gt;"));
    assert!(result.contains("&amp;"));
    assert!(result.contains("&quot;characters&quot;"));
}

#[test]
fn formats_multiple_skills() {
    let skills = vec![
        create_test_skill(
            "skill-one",
            "First skill.",
            "/path/one/SKILL.md",
            "/path/one",
            false,
            "test",
        ),
        create_test_skill(
            "skill-two",
            "Second skill.",
            "/path/two/SKILL.md",
            "/path/two",
            false,
            "test",
        ),
    ];
    let result = format_skills_for_prompt(&skills, FileReadTool::Read);
    assert!(result.contains("<name>skill-one</name>"));
    assert!(result.contains("<name>skill-two</name>"));
    assert_eq!(result.matches("<skill>").count(), 2);
}

#[test]
fn excludes_skills_with_disable_model_invocation_from_prompt() {
    let skills = vec![
        create_test_skill(
            "visible-skill",
            "A visible skill.",
            "/path/visible/SKILL.md",
            "/path/visible",
            false,
            "test",
        ),
        create_test_skill(
            "hidden-skill",
            "A hidden skill.",
            "/path/hidden/SKILL.md",
            "/path/hidden",
            true,
            "test",
        ),
    ];
    let result = format_skills_for_prompt(&skills, FileReadTool::Read);
    assert!(result.contains("<name>visible-skill</name>"));
    assert!(!result.contains("<name>hidden-skill</name>"));
    assert_eq!(result.matches("<skill>").count(), 1);
}

#[test]
fn returns_empty_string_when_all_skills_have_disable_model_invocation() {
    let skills = vec![create_test_skill(
        "hidden-skill",
        "A hidden skill.",
        "/path/hidden/SKILL.md",
        "/path/hidden",
        true,
        "test",
    )];
    assert_eq!(format_skills_for_prompt(&skills, FileReadTool::Read), "");
}

// --- loadSkills with options -------------------------------------------------

#[test]
fn loads_from_explicit_skill_paths() {
    let empty_agent_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-agent");
    let empty_cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-cwd");
    let result = load_skills(&LoadSkillsOptions {
        agent_dir: empty_agent_dir.clone(),
        cwd: empty_cwd,
        skill_paths: vec![fixtures_dir().join("valid-skill")],
        include_defaults: true,
    });
    assert_eq!(result.skills.len(), 1);
    // Explicit paths are `path` sources, whose scope is temporary.
    assert_eq!(result.skills[0].scope, Scope::Temporary);
    assert_eq!(warnings(&result).len(), 0);
    let _ = empty_agent_dir;
}

#[test]
fn warns_when_skill_path_does_not_exist() {
    let empty_agent_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-agent");
    let empty_cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-cwd");
    let result = load_skills(&LoadSkillsOptions {
        agent_dir: empty_agent_dir,
        cwd: empty_cwd,
        skill_paths: vec!["/non/existent/path".into()],
        include_defaults: true,
    });
    assert_eq!(result.skills.len(), 0);
    assert!(
        warnings(&result)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not exist")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn expands_tilde_in_skill_paths() {
    // `~/.pi/agent/skills` and its spelled-out form load the same set.
    let empty_agent_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-agent");
    let empty_cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-cwd");
    let home = std::env::var("HOME").unwrap_or_default();
    let with_tilde = load_skills(&LoadSkillsOptions {
        agent_dir: empty_agent_dir.clone(),
        cwd: empty_cwd.clone(),
        skill_paths: vec!["~/.pi/agent/skills".into()],
        include_defaults: true,
    });
    let without_tilde = load_skills(&LoadSkillsOptions {
        agent_dir: empty_agent_dir,
        cwd: empty_cwd,
        skill_paths: vec![PathBuf::from(&home).join(".pi/agent/skills")],
        include_defaults: true,
    });
    assert_eq!(with_tilde.skills.len(), without_tilde.skills.len());
}

#[test]
fn loads_project_and_user_defaults() {
    // A cwd with a .pi/skills tree loads them with the project scope, next to
    // the (empty) user dir.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let project_skill = temp.path().join(".pi/skills/my-skill");
    std::fs::create_dir_all(&project_skill).expect("skill dir");
    std::fs::write(
        project_skill.join("SKILL.md"),
        "---\nname: project-skill\ndescription: A project skill.\n---\nBody",
    )
    .expect("SKILL.md");
    let empty_agent_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-agent");
    let result = load_skills(&LoadSkillsOptions {
        agent_dir: empty_agent_dir,
        cwd: temp.path().to_path_buf(),
        skill_paths: Vec::new(),
        include_defaults: true,
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "project-skill");
    assert_eq!(result.skills[0].scope, Scope::Project);
    assert_eq!(warnings(&result).len(), 0);
}

// --- collision handling ------------------------------------------------------

#[test]
fn detects_name_collisions_and_keeps_the_first_skill() {
    let first = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: collision_fixtures_dir().join("first"),
        source: "first".to_string(),
    });
    let second = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir: collision_fixtures_dir().join("second"),
        source: "second".to_string(),
    });

    // The collision behavior from loadSkills() (`skills.ts:426-455`), which
    // the TS test simulates over the two scans: the first skill scanned for a
    // name wins, the later one is reported and dropped.
    let mut skill_map: std::collections::HashMap<String, &Skill> = std::collections::HashMap::new();
    let mut collision_warnings: Vec<(PathBuf, String)> = Vec::new();
    for skill in &first.skills {
        skill_map.insert(skill.name.clone(), skill);
    }
    for skill in &second.skills {
        let existing = skill_map.get(&skill.name);
        match existing {
            Some(existing) => collision_warnings.push((
                skill.file_path.clone(),
                format!(
                    "name collision: \"{}\" already loaded from {}",
                    skill.name,
                    existing.file_path.display()
                ),
            )),
            None => {
                skill_map.insert(skill.name.clone(), skill);
            }
        }
    }

    assert_eq!(skill_map.len(), 1);
    assert_eq!(skill_map["calendar"].source, "first");
    assert_eq!(collision_warnings.len(), 1);
    assert!(
        collision_warnings[0].1.contains("name collision"),
        "{}",
        collision_warnings[0].1
    );

    // The real loadSkills() run: explicit skill paths are tagged with the
    // "path" source (`skills.ts:465-471`), the first scan wins, and the
    // collision comes back as a diagnostic.
    let empty_agent_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-agent");
    let empty_cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/empty-cwd");
    let result = load_skills(&LoadSkillsOptions {
        agent_dir: empty_agent_dir,
        cwd: empty_cwd,
        skill_paths: vec![
            collision_fixtures_dir().join("first"),
            collision_fixtures_dir().join("second"),
        ],
        include_defaults: false,
    });
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].source, "path");
    assert_eq!(result.skills[0].name, "calendar");
    let collisions: Vec<&ResourceDiagnostic> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic.kind, DiagnosticKind::Collision))
        .collect();
    assert_eq!(collisions.len(), 1);
    assert!(
        collisions[0].message.contains("collision"),
        "{}",
        collisions[0].message
    );
    assert!(collisions[0].message.contains("calendar"));
    // The first/second scans themselves each see one skill.
    assert_eq!(first.skills.len(), 1);
    assert_eq!(second.skills.len(), 1);
}
