//! Pi coding agent CLI (Rust port of `@earendil-works/pi-coding-agent`).
//!
//! Binary `pi`. P0 stub: argument parsing only. Modes and tools land in P4+.

use clap::Parser;

/// Pi coding agent (Rust port, work in progress).
#[derive(Parser, Debug)]
#[command(name = "pi", version, about)]
struct Cli {
    /// Print mode: run a prompt non-interactively and print the final response
    #[arg(short = 'p')]
    print: bool,
    /// The prompt (interactive mode when omitted)
    prompt: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    if cli.print {
        eprintln!("pi-rs: print mode not implemented yet (phase P4)");
    } else {
        eprintln!("pi-rs: interactive mode not implemented yet (phase P6)");
    }
    std::process::exit(2);
}
