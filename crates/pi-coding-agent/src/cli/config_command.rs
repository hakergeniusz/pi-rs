//! `pi config` (`package-manager-cli.ts:306-315`, `876-960`; routed in
//! `main.ts:661` before argument parsing).
//!
//! Upstream opens a resource-configuration TUI that toggles package resources
//! in global/project settings. That TUI has no port in pi-rs; this module
//! keeps the routing, the exact option/usage diagnostics, and the project-trust
//! gate, then reports the limitation instead of letting `config` fall through
//! into the prompt text.

use crate::config::settings::{SettingsManager, SettingsManagerCreateOptions};
use crate::config::{APP_NAME, PROJECT_CONFIG_DIR_NAME, get_agent_dir, get_settings_path};

const CONFIG_COMMAND_USAGE: &str = "pi config [-l] [--approve|--no-approve]";

fn print_config_help() {
    println!("Usage:");
    println!("  {CONFIG_COMMAND_USAGE}");
    println!();
    println!("Open the resource configuration TUI to enable or disable package resources.");
    println!(
        "Without -l, starts in global settings ({}).",
        get_settings_path().display()
    );
    println!("Press Tab in the TUI to switch between global and project-local modes.");
    println!();
    println!("Options:");
    println!(
        "  -l, --local       Edit project overrides ({PROJECT_CONFIG_DIR_NAME}/settings.json)"
    );
    println!("  -a, --approve     Trust project-local files for this command with -l");
    println!("  -na, --no-approve Ignore project-local files for this command with -l");
}

/// The `handleConfigCommand` entry (`package-manager-cli.ts:876-960`).
pub fn handle_config_command(raw: &[String]) -> Option<i32> {
    if raw.first().is_none_or(|word| word != "config") {
        return None;
    }
    Some(run(&raw[1..]))
}

fn run(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_config_help();
        return 0;
    }
    let mut local = false;
    let mut trust_override: Option<bool> = None;
    for arg in args {
        match arg.as_str() {
            "-l" | "--local" => local = true,
            "-a" | "--approve" => trust_override = Some(true),
            "-na" | "--no-approve" => trust_override = Some(false),
            other if other.starts_with('-') => {
                eprintln!("Unknown option {other} for \"config\".");
                eprintln!("Use \"{APP_NAME} --help\" or \"{CONFIG_COMMAND_USAGE}\".");
                return 1;
            }
            other => {
                eprintln!("Unexpected argument {other}.");
                eprintln!("Usage: {CONFIG_COMMAND_USAGE}");
                return 1;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let agent_dir = get_agent_dir();
    let settings = SettingsManager::create(
        &cwd,
        &agent_dir,
        SettingsManagerCreateOptions {
            project_trusted: trust_override,
        },
    );
    if local && !settings.is_project_trusted() {
        eprintln!("Project is not trusted. Use --approve to modify local resource config.");
        return 1;
    }
    // Honest deferral: no `config` resource TUI exists in the port yet. The
    // files it would edit are reported so the command is still actionable.
    eprintln!("pi-rs: the resource configuration TUI is not available in this build.");
    let target = if local {
        cwd.join(PROJECT_CONFIG_DIR_NAME).join("settings.json")
    } else {
        get_settings_path()
    };
    eprintln!(
        "Edit {} directly to enable or disable resources.",
        target.display()
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_only_the_config_word() {
        assert!(handle_config_command(&["config".to_string()]).is_some());
        assert!(handle_config_command(&["config".to_string(), "--help".to_string()]).is_some());
        assert!(handle_config_command(&["config-file".to_string()]).is_none());
    }
}
