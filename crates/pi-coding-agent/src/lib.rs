//! Pi coding agent core (Rust port of `@earendil-works/pi-coding-agent`).
//!
//! Ground truth: `packages/coding-agent/src` in earendil-works/pi. Module
//! docs cite the TS file and line range each piece mirrors.
//!
//! Layout (Phase P4, non-interactive core):
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

pub mod auth;
pub mod cli;
pub mod config;
pub mod extensions;
pub mod modes;
pub mod prompt;
pub mod prompt_templates;
pub mod runtime;
pub mod session;
pub mod skills;
pub mod tools;
