//! Thin entry point. All real logic lives in `lib.rs` so it can be unit-tested directly,
//! without spawning a process for every test case.

use std::io::{IsTerminal, Write};

fn main() {
    let exe_path = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let args = match backstar_restore_lib::parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            // Exit code 2 is the usage-error slot (see the EXIT CODES section of --help).
            std::process::exit(2);
        }
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // The D5 overwrite prompt is asked only on a real terminal; piped stdin gets the
    // refusal path instead (see `overwrite_policy` in lib.rs).
    let stdin = std::io::stdin();
    let stdin_is_tty = stdin.is_terminal();
    let mut input = stdin.lock();

    match backstar_restore_lib::run(&args, &exe_path, &mut out, &mut input, stdin_is_tty) {
        Ok(code) => {
            let _ = out.flush();
            std::process::exit(code);
        }
        Err(e) => {
            let _ = out.flush();
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
