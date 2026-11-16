//! Pi coding agent core (Rust port of `@earendil-works/pi-coding-agent`).
//!
//! Ground truth: `packages/coding-agent/src` in earendil-works/pi. Module
//! docs cite the TS file and line range each piece mirrors.
//!
//! Layout:
//!
//! * [`config`] — global + project config loading (`config.ts`).
//! * [`auth`] — credential storage for provider auth (`core/auth-storage.ts`).
//! * [`session`] — the JSONL session tree, v3 format + migrations
//!   (`core/session-manager.ts`).
//! * [`tools`] — the built-in tools: read, bash, edit, write, ls, find,
//!   grep, truncate (`core/tools/`).
//! * [`prompt`] — system prompt assembly (`core/system-prompt.ts`).
//! * [`prompt_templates`] — command prompt templates
//!   (`core/prompt-templates.ts`).
//! * [`skills`] — skill discovery and packaging (`core/skills.ts`).
//! * [`cli`] — argument parsing, `@file` processing, project trust
//!   (`cli/args.ts`, `cli/file-processor.ts`, `cli/project-trust.ts`).
//! * [`modes`] — print, JSON-event, RPC, and interactive modes (`modes/`).
//! * [`extensions`] — runtime-agnostic extension host seam
//!   (`extensions/index.ts` port decision pending at the P5 gate).

#![forbid(unsafe_code)]

/// Credential storage for provider auth (`core/auth-storage.ts`).
pub mod auth;
/// Changelog parsing and link normalization (`utils/changelog.ts`).
pub mod changelog;
/// Argument parsing, `@file` processing, project trust.
pub mod cli;
/// Clipboard writes for `/copy` (`utils/clipboard-command.ts` + `utils/clipboard.ts`).
pub mod clipboard;
/// Global + project config loading (`config.ts`).
pub mod config;
/// Runtime-agnostic extension host seam (runtime decision pending at the P5 gate).
pub mod extensions;
/// models.json — the credential-blind provider/model configuration layer
/// (`core/model-config.ts` plus the models.json halves of
/// `core/provider-composer.ts`).
pub mod models_config;
/// The composed model catalog: base catalog ⊕ models.json ⊕ extension
/// providers ⊕ virtual models (`core/model-runtime.ts`, slice 6b).
pub mod models_state;
/// Print, JSON-event, RPC, and interactive modes.
pub mod modes;
/// System prompt assembly (`core/system-prompt.ts`).
pub mod prompt;
/// Command prompt templates (`core/prompt-templates.ts`).
pub mod prompt_templates;
/// Process-wide tokio runtime driving async work from the sync CLI paths.
pub mod runtime;
/// The JSONL session tree: v3 records, store, migrations, compaction.
pub mod session;
/// Skill discovery and packaging (`core/skills.ts`).
pub mod skills;
/// The built-in tools — read, bash, edit, write, ls, find, grep — plus renderers.
pub mod tools;
/// Virtual models: catalog entries routing each request to a physical model
/// (`core/virtual-models.ts` plus the routing consumers).
pub mod virtual_models;
