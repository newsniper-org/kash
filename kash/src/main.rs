//! kash — Korn Again SHell CLI entry point.
//!
//! Responsibilities:
//!
//!   * Inspect `argv[0]` to pick an initial mode and CLI dialect:
//!     `sh` → `posix-strict`, `ksh` / `ksh93` → `ksh93u-strict`,
//!     `kash` (or anything else) → `default`. Symlink-invoked
//!     dialects reject kash-specific flags (`--mode=`, …) to
//!     preserve drop-in compatibility per
//!     `project_shell_mode_syntax.md`.
//!   * Parse the surface CLI: `--mode=<name>`, `-c COMMAND`, and a
//!     trailing script path with positional args.
//!   * Read source from `-c`'s body, the script path, or stdin.
//!   * Parse + evaluate against a `kash-core` `Evaluator`. Print
//!     the captured stdout buffer to real stdout, surface
//!     evaluator errors on stderr, and propagate the exit status.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use kash_core::eval::Evaluator;
use kash_core::mode::{BaseMode, Mode};
use kash_core::parser::parse;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let invocation = Path::new(args.first().map(String::as_str).unwrap_or("kash"))
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("kash")
        .to_string();
    let (initial_mode, allow_kash_flags) = match invocation.as_str() {
        "sh" => (Mode::new(BaseMode::PosixStrict), false),
        "ksh" | "ksh93" => (Mode::new(BaseMode::Ksh93uStrict), false),
        _ => (Mode::default_mode(), true),
    };

    let parsed = match parse_cli(&args[1..], &invocation, allow_kash_flags) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let mode = match parsed.mode_override.as_deref() {
        Some(spec) => match Mode::parse(spec) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{invocation}: {e}");
                return ExitCode::from(2);
            }
        },
        None => initial_mode,
    };

    let source = match read_source(&parsed, &invocation) {
        Ok(s) => s,
        Err(code) => return code,
    };

    let prog = match parse(&source) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{invocation}: parse error: {e}");
            return ExitCode::from(2);
        }
    };
    let mut ev = Evaluator::<kash_core::collections::BTreeBackend>::with_mode(mode);
    ev.set_positionals(parsed.positional);
    let outcome = ev.eval_program(&prog);
    let captured = ev.take_output();
    print!("{captured}");
    let _ = io::stdout().flush();
    match outcome {
        Ok(o) => ExitCode::from(clamp_status(o.status())),
        Err(e) => {
            eprintln!("{invocation}: {e}");
            ExitCode::from(clamp_status(e.exit_code()))
        }
    }
}

/// Surface-level parse of the CLI args (everything after `argv[0]`).
struct CliInvocation {
    mode_override: Option<String>,
    command_arg: Option<String>,
    script_path: Option<String>,
    positional: Vec<String>,
}

fn parse_cli(
    rest: &[String],
    invocation: &str,
    allow_kash_flags: bool,
) -> Result<CliInvocation, ExitCode> {
    let mut out = CliInvocation {
        mode_override: None,
        command_arg: None,
        script_path: None,
        positional: Vec::new(),
    };
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        if let Some(spec) = arg.strip_prefix("--mode=") {
            if !allow_kash_flags {
                eprintln!(
                    "{invocation}: `--mode=` is a kash-specific flag; \
                     invoke the canonical `kash` instead"
                );
                return Err(ExitCode::from(2));
            }
            out.mode_override = Some(spec.to_string());
            i += 1;
            continue;
        }
        if arg == "-c" {
            i += 1;
            if i >= rest.len() {
                eprintln!("{invocation}: -c requires an argument");
                return Err(ExitCode::from(2));
            }
            out.command_arg = Some(rest[i].clone());
            i += 1;
            // POSIX: an optional `command_name` plus further positional
            // args may follow `-c BODY`.
            out.positional.extend(rest[i..].iter().cloned());
            return Ok(out);
        }
        if arg == "--" {
            i += 1;
            out.positional.extend(rest[i..].iter().cloned());
            return Ok(out);
        }
        if arg.starts_with('-') {
            eprintln!("{invocation}: unsupported flag `{arg}`");
            return Err(ExitCode::from(2));
        }
        // First non-flag, non-`--` arg is the script path; the rest
        // are its positional parameters.
        out.script_path = Some(arg.clone());
        i += 1;
        out.positional.extend(rest[i..].iter().cloned());
        return Ok(out);
    }
    Ok(out)
}

fn read_source(invocation: &CliInvocation, name: &str) -> Result<String, ExitCode> {
    if let Some(cmd) = &invocation.command_arg {
        return Ok(cmd.clone());
    }
    if let Some(path) = &invocation.script_path {
        match fs::read_to_string(path) {
            Ok(s) => return Ok(s),
            Err(e) => {
                eprintln!("{name}: {path}: {e}");
                return Err(ExitCode::from(1));
            }
        }
    }
    let mut buf = String::new();
    match io::stdin().read_to_string(&mut buf) {
        Ok(_) => Ok(buf),
        Err(e) => {
            eprintln!("{name}: stdin: {e}");
            Err(ExitCode::from(1))
        }
    }
}

/// Shells return exit statuses in `0..=255`. Clamp anything wider —
/// e.g. evaluator NotFound (`127`), our custom mode errors — into
/// that range. Negative codes (signal-style) get folded into the
/// POSIX `128 + n` representation.
fn clamp_status(code: i32) -> u8 {
    if code < 0 {
        (128i32 + code.abs()).min(255) as u8
    } else {
        (code & 0xff) as u8
    }
}

