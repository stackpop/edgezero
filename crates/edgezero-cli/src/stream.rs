//! CLI stream-discipline helpers.
//!
//! `edgezero`'s user-facing output splits between two streams:
//!   - **stdout** — machine-consumable payload (diff bodies, push
//!     JSON envelopes, `config diff --format json`).
//!   - **stderr** — human-facing narration (prompts, `# no changes`
//!     notes, `# warning: remote changed…` messages). Kept OFF stdout
//!     so pipes like `<cli> config diff | jq` don't choke on the
//!     narration.
//!
//! Every stderr write inside `edgezero-cli` MUST go through the
//! helpers below. The workspace `clippy::print_stderr` restriction
//! then catches accidental `eprint!` / `eprintln!` in ANY other call
//! site as a real bug (the mistaken use of stderr for a payload).

use std::io::{Write as _, stderr};

/// Write `msg` + newline to stderr. Best-effort: write errors on a
/// closed stderr are swallowed because the caller has no useful
/// recovery — the message is informational.
#[inline]
pub fn info_line(msg: &str) {
    let handle = stderr();
    let mut locked = handle.lock();
    drop(writeln!(locked, "{msg}"));
}

/// Write `msg` to stderr WITHOUT a trailing newline and flush. Keeps
/// the cursor on the prompt line for the operator's y/N input.
#[inline]
pub fn prompt(msg: &str) {
    let handle = stderr();
    let mut locked = handle.lock();
    drop(write!(locked, "{msg}"));
    drop(locked.flush());
}

/// Write `msg` + newline to stdout. The workspace
/// `clippy::print_stdout` restriction then catches accidental
/// `println!` elsewhere as a real bug: stdout is reserved for
/// machine-readable CLI payloads (diff bodies, JSON envelopes,
/// pipe-friendly output).
#[inline]
pub fn stdout_line(msg: &str) {
    use std::io::stdout;
    let handle = stdout();
    let mut locked = handle.lock();
    drop(writeln!(locked, "{msg}"));
}
