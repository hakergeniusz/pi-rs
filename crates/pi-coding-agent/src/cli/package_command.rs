//! The package subcommands — `pi install|remove|uninstall|list|update`
//! (`package-manager-cli.ts`'s `parsePackageCommand` + `handlePackageCommand`,
//! routed in `main.ts:648` before mode startup). The manager is
//! [`crate::package_manager`]; `update` (self-update, model catalogs and the
//! batch updater) still reports the honest deferral (see HANDOFF).
//!
//! Output is plain: upstream's `chalk` degrades to plain on non-TTY
//! stdout and these commands run before any TUI exists.

use std::path::PathBuf;

use crate::cli::project_trust::{
    DefaultProjectTrust, ProjectTrustStore, ResolveProjectTrustedOptions, resolve_project_trusted,
};
use crate::config::settings::{SettingsManager, SettingsManagerCreateOptions};
use crate::config::{APP_NAME, PROJECT_CONFIG_DIR_NAME};
use crate::package_manager::PackageManager;

/// `PackageCommand` (`package-manager-cli.ts:51`); `uninstall` parses as
/// `remove`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageCommand {
    Install,
    Remove,
    List,
    Update,
}

impl PackageCommand {
    fn as_str(self) -> &'static str {
        match self {
            PackageCommand::Install => "install",
            PackageCommand::Remove => "remove",
            PackageCommand::List => "list",
            PackageCommand::Update => "update",
        }
    }
}

/// `PackageCommandOptions` (`package-manager-cli.ts:283-296`).
#[derive(Debug, Default)]
struct PackageCommandOptions {
    command: Option<PackageCommand>,
    source: Option<String>,
    local: bool,
    project_trust_override: Option<bool>,
    help: bool,
    invalid_option: Option<String>,
    invalid_argument: Option<String>,
    conflicting_options: Option<String>,
}

/// `parsePackageCommand` (`:417-580`): `None` when the first argument is
/// not a package command.
fn parse_package_command(args: &[String]) -> Option<PackageCommandOptions> {
    let (raw_command, rest) = args.split_first()?;
    let command = match raw_command.as_str() {
        "uninstall" | "remove" => PackageCommand::Remove,
        "install" => PackageCommand::Install,
        "update" => PackageCommand::Update,
        "list" => PackageCommand::List,
        _ => return None,
    };

    let mut options = PackageCommandOptions {
        command: Some(command),
        ..PackageCommandOptions::default()
    };
    for arg in rest {
        match arg.as_str() {
            "-h" | "--help" => options.help = true,
            "-l" | "--local" => {
                if matches!(command, PackageCommand::Install | PackageCommand::Remove) {
                    options.local = true;
                } else if options.invalid_option.is_none() {
                    options.invalid_option = Some(arg.clone());
                }
            }
            "--self" | "--extensions" | "--models" | "--all" | "--force" => {
                // (the `--force` flag only matters on the deferred update
                // path; it parses to keep the unknown-option validation
                // faithful)
                // The update-only flags still validate per-command (the
                // update case answers the deferral afterwards).
                if command != PackageCommand::Update && options.invalid_option.is_none() {
                    options.invalid_option = Some(arg.clone());
                }
            }
            "--approve" | "-a" => options.project_trust_override = Some(true),
            "--no-approve" | "-na" => options.project_trust_override = Some(false),
            "--extension" => {
                // A valued flag only `update` accepts; the value
                // consumption is unnecessary for the deferral answer.
                if command != PackageCommand::Update && options.invalid_option.is_none() {
                    options.invalid_option = Some(arg.clone());
                }
            }
            other => {
                if other.starts_with('-') {
                    if options.invalid_option.is_none() {
                        options.invalid_option = Some(other.to_string());
                    }
                } else if options.source.is_none() {
                    options.source = Some(other.to_string());
                } else if options.invalid_argument.is_none() {
                    options.invalid_argument = Some(other.to_string());
                }
            }
        }
    }
    Some(options)
}

/// `getPackageCommandUsage` (`:307-317`).
fn package_command_usage(command: PackageCommand) -> String {
    match command {
        PackageCommand::Install => {
            format!("{APP_NAME} install <source> [-l] [--approve|--no-approve]")
        }
        PackageCommand::Remove => {
            format!("{APP_NAME} remove <source> [-l] [--approve|--no-approve]")
        }
        PackageCommand::Update => {
            format!(
                "{APP_NAME} update [source|self|pi] [--self|--extensions|--models|--all] \
                 [--extension <source>] [--approve|--no-approve] [--force]"
            )
        }
        PackageCommand::List => format!("{APP_NAME} list [--approve|--no-approve]"),
    }
}

fn usage_line(command: PackageCommand) -> String {
    format!("Usage: {}", package_command_usage(command))
}

/// `printPackageCommandHelp` (`:337-...`), verbatim bodies.
fn print_package_command_help(command: PackageCommand) {
    match command {
        PackageCommand::Install => {
            println!(
                "Usage:\n  {}\n\nInstall a package and add it to settings.\n\nOptions:\n  \
                 -l, --local       Install project-locally ({PROJECT_CONFIG_DIR_NAME}/settings.json)\n  \
                 -a, --approve     Trust project-local files for this command\n  \
                 -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  \
                 {APP_NAME} install npm:@foo/bar\n  \
                 {APP_NAME} install git:github.com/user/repo\n  \
                 {APP_NAME} install git:git@github.com:user/repo\n  \
                 {APP_NAME} install https://github.com/user/repo\n  \
                 {APP_NAME} install ssh://git@github.com/user/repo\n  \
                 {APP_NAME} install ./local/path\n",
                package_command_usage(command)
            );
        }
        PackageCommand::Remove => {
            println!(
                "Usage:\n  {}\n\nRemove a package and its source from settings.\nAlias: \
                 {APP_NAME} uninstall <source> [-l]\n\nOptions:\n  \
                 -l, --local       Remove from project settings ({PROJECT_CONFIG_DIR_NAME}/settings.json)\n  \
                 -a, --approve     Trust project-local files for this command\n  \
                 -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  \
                 {APP_NAME} remove npm:@foo/bar\n  \
                 {APP_NAME} remove git:github.com/user/repo\n  \
                 {APP_NAME} remove ./local/path\n",
                package_command_usage(command)
            );
        }
        PackageCommand::List => {
            println!(
                "Usage:\n  {}\n\nList installed packages from settings.\n\nOptions:\n  \
                 -a, --approve     Trust project-local files for this command\n  \
                 -na, --no-approve Ignore project-local files for this command\n",
                package_command_usage(command)
            );
        }
        PackageCommand::Update => {
            println!(
                "Usage:\n  {}\n\nUpdate pi, installed packages, or model catalogs.\n\nOptions:\n  \
                 --self             Update pi itself only\n  \
                 --extensions       Update all installed packages\n  \
                 --models           Refresh model catalogs\n  \
                 --all              Update pi and all packages\n  \
                 --extension <src>  Update one package\n  \
                 -a, --approve      Trust project-local files for this command\n  \
                 -na, --no-approve  Ignore project-local files for this command\n  \
                 --force            Redo the update even when already current\n",
                package_command_usage(command)
            );
        }
    }
}

/// The command failure exit: `Error: {message}` on stderr (upstream's
/// install failures surface as an unhandled promise rejection from the
/// uncalled-catch entry; the port prints the message — HANDOFF deviation).
fn command_error(command: PackageCommand, message: &str) -> i32 {
    eprintln!("Error: {message}");
    eprintln!("{}", usage_line(command));
    1
}

/// `handlePackageCommand` (`:975-1140`): `None` when the arguments are
/// not a package command, else the process exit code.
#[allow(clippy::too_many_lines)] // mirrors the TS handlePackageCommand; split when extending
pub fn handle_package_command(args: &[String]) -> Option<i32> {
    let options = parse_package_command(args)?;
    let command = options.command.expect("command set by parse");
    let usage = || usage_line(command);

    if options.help {
        print_package_command_help(command);
        return Some(0);
    }

    if let Some(invalid_option) = &options.invalid_option {
        eprintln!(
            "Unknown option {invalid_option} for \"{}\".",
            command.as_str()
        );
        eprintln!("Use \"{APP_NAME} --help\" or \"{}\".", usage());
        return Some(1);
    }

    if let Some(invalid_argument) = &options.invalid_argument {
        eprintln!("Unexpected argument {invalid_argument}.");
        eprintln!("{}", usage());
        return Some(1);
    }

    if let Some(conflicting_options) = &options.conflicting_options {
        eprintln!("{conflicting_options}");
        eprintln!("{}", usage());
        return Some(1);
    }

    let source = options.source.clone();
    if matches!(command, PackageCommand::Install | PackageCommand::Remove) && source.is_none() {
        eprintln!("Missing {} source.", command.as_str());
        eprintln!("{}", usage());
        return Some(1);
    }

    if command == PackageCommand::Update {
        // The npm/self-update machinery is deferred (`pi update` upstream
        // drives the managed installer, extension updates, and model
        // catalog refreshes).
        eprintln!("Error: update is not implemented in pi-rs yet");
        return Some(1);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let agent_dir = crate::config::get_agent_dir();
    let writes_project_package_config =
        matches!(command, PackageCommand::Install | PackageCommand::Remove) && options.local;

    // `createCommandSettingsManager` (`:842-888`): bootstrap the global
    // settings for `defaultProjectTrust`, resolve the trust ladder with
    // the `--approve` override, then build the command's manager.
    let bootstrap = SettingsManager::create(
        &cwd,
        &agent_dir,
        SettingsManagerCreateOptions {
            project_trusted: Some(false),
        },
    );
    let default_project_trust = startup_default_project_trust(&bootstrap);
    let store = ProjectTrustStore::new(&agent_dir);
    let project_trusted = resolve_project_trusted(&mut ResolveProjectTrustedOptions {
        cwd: &cwd,
        store: &store,
        trust_override: options.project_trust_override,
        default_project_trust,
        extension_trust: None,
    })
    .unwrap_or(false);

    let mut settings = SettingsManager::create(
        &cwd,
        &agent_dir,
        SettingsManagerCreateOptions {
            project_trusted: Some(project_trusted),
        },
    );
    report_settings_errors(&mut settings);
    if !settings.is_project_trusted() && writes_project_package_config {
        eprintln!("Project is not trusted. Use --approve to modify local package config.");
        return Some(1);
    }

    let mut manager = PackageManager::new(&cwd, &agent_dir, &mut settings);
    match command {
        PackageCommand::Install => {
            let source = source.expect("install requires a source");
            println!("Installing {source}...");
            let result = manager.install_and_persist(&source, options.local);
            settings.flush();
            report_settings_errors(&mut settings);
            match result {
                Ok(()) => {
                    println!("Installed {source}");
                    Some(0)
                }
                Err(message) => Some(command_error(command, &message)),
            }
        }
        PackageCommand::Remove => {
            let source = source.expect("remove requires a source");
            let result = manager.remove_and_persist(&source, options.local);
            settings.flush();
            report_settings_errors(&mut settings);
            match result {
                Ok(true) => {
                    println!("Removed {source}");
                    Some(0)
                }
                Ok(false) => {
                    eprintln!("No matching package found for {source}");
                    Some(1)
                }
                Err(message) => Some(command_error(command, &message)),
            }
        }
        PackageCommand::List => match manager.list_configured_packages() {
            Ok(configured) => {
                if configured.is_empty() {
                    println!("No packages installed.");
                    return Some(0);
                }
                let user_packages: Vec<_> = configured
                    .iter()
                    .filter(|pkg| pkg.scope == crate::package_manager::Scope::User)
                    .collect();
                let project_packages: Vec<_> = configured
                    .iter()
                    .filter(|pkg| pkg.scope == crate::package_manager::Scope::Project)
                    .collect();
                let format_package = |pkg: &crate::package_manager::ConfiguredPackage| {
                    let display = if pkg.filtered {
                        format!("{} (filtered)", pkg.source)
                    } else {
                        pkg.source.clone()
                    };
                    println!("  {display}");
                    if let Some(installed_path) = &pkg.installed_path {
                        println!("    {}", installed_path.display());
                    }
                };
                if !user_packages.is_empty() {
                    println!("User packages:");
                    for pkg in &user_packages {
                        format_package(pkg);
                    }
                }
                if !project_packages.is_empty() {
                    if !user_packages.is_empty() {
                        println!();
                    }
                    println!("Project packages:");
                    for pkg in &project_packages {
                        format_package(pkg);
                    }
                }
                Some(0)
            }
            Err(message) => Some(command_error(command, &message)),
        },
        PackageCommand::Update => unreachable!("handled above"),
    }
}

/// `reportSettingsErrors` (`package-manager-cli.ts:299-306`).
fn report_settings_errors(settings: &mut SettingsManager) {
    for error in settings.drain_errors() {
        eprintln!(
            "Warning (package command, {} settings): {}",
            error.scope.as_str(),
            error.message
        );
    }
}

/// The `defaultProjectTrust` setting (`startup_default_project_trust`'s
/// read over the bootstrap manager).
fn startup_default_project_trust(settings: &SettingsManager) -> Option<DefaultProjectTrust> {
    match settings
        .get_global_settings()
        .get("defaultProjectTrust")
        .and_then(serde_json::Value::as_str)
    {
        Some("always") => Some(DefaultProjectTrust::Always),
        Some("never") => Some(DefaultProjectTrust::Never),
        Some("ask") => Some(DefaultProjectTrust::Ask),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_string()).collect()
    }

    /// `parsePackageCommand`'s routing: the `uninstall` alias, the flags,
    /// and the error states the handler reports.
    #[test]
    fn parse_routes_the_command_words_and_flags() {
        let options = parse_package_command(&args(&["uninstall", "npm:x", "-l"])).expect("command");
        assert_eq!(options.command, Some(PackageCommand::Remove));
        assert_eq!(options.source.as_deref(), Some("npm:x"));
        assert!(options.local);
        assert!(
            parse_package_command(&args(&["install", "./ext", "-l"]))
                .expect("command")
                .local
        );
        assert!(
            parse_package_command(&args(&["install", "./ext", "-a"]))
                .expect("command")
                .project_trust_override
                .is_some()
        );
        // `-l` on list is an unknown option (`invalidOption`).
        let options = parse_package_command(&args(&["list", "-l"])).expect("command");
        assert_eq!(options.invalid_option.as_deref(), Some("-l"));
        // A second positional is an unexpected argument.
        let options = parse_package_command(&args(&["install", "a", "b"])).expect("command");
        assert_eq!(options.invalid_argument.as_deref(), Some("b"));
        // Not a package command: any other first word.
        assert!(parse_package_command(&args(&["config", "-l"])).is_none());
        assert!(parse_package_command(&args(&["-p", "hi"])).is_none());
    }
}
