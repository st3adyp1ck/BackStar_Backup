//! Thin entry point. All real logic lives in `lib.rs` so it can be unit-tested directly,
//! without spawning a process for every test case.

use std::io::Write;

fn main() {
    let exe_path = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let args = match backstar_restore_lib::parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    match backstar_restore_lib::run(&args, &exe_path, &mut out) {
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
