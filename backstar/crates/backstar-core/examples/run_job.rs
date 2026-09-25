//! Drive a real backup from the command line, for validating the engine against real trees.
//!
//! ```text
//! cargo run --release --example run_job -- <source> [<source>...] --dest <dir>
//! ```

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use backstar_core::config::{Job, JobKind};
use backstar_core::engine;
use backstar_core::events::Event;
use backstar_core::exclude::ExcludeRules;

fn human(bytes: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const KB: u64 = 1 << 10;
    match bytes {
        b if b >= GB => format!("{:.2} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.0} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(split) = args.iter().position(|a| a == "--dest") else {
        eprintln!("usage: run_job <source>... --dest <dir> [--project-excludes]");
        std::process::exit(2);
    };

    let sources: Vec<PathBuf> = args[..split].iter().map(PathBuf::from).collect();
    let dest = PathBuf::from(&args[split + 1]);
    let project_excludes = args.iter().any(|a| a == "--project-excludes");

    let job = Job {
        id: "bench".into(),
        name: "Bench".into(),
        sources,
        dest: dest.clone(),
        dest_portable: None,
        kind: JobKind::Snapshot,
        excludes: if project_excludes { ExcludeRules::project() } else { ExcludeRules::none() },
        presets: vec![],
        git_gc: false,
        enabled: true,
        schedule: None,
    };

    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let mut scan_reports = 0u64;
    let mut progress_events = 0u64;
    let mut last_plan = None;

    // No retention pruning in this bench harness: pass None (production reads
    // Config.keep_snapshots).
    let result = engine::run_job(&job, None, &cancel, &mut |e| match e {
        Event::ScanProgress { files_seen, bytes_seen } => {
            scan_reports += 1;
            if scan_reports % 20 == 0 {
                println!("  scanning: {files_seen} files, {}", human(bytes_seen));
            }
        }
        Event::PlanReady { to_copy, to_link, bytes_to_copy, .. } => {
            println!(
                "  plan: copy {to_copy} ({}), link {to_link}",
                human(bytes_to_copy)
            );
            last_plan = Some((to_copy, to_link));
        }
        Event::FileProgress { .. } => progress_events += 1,
        Event::VerifyDone { examined, mismatches } => {
            println!("  verified {examined} file(s), {} mismatch(es)", mismatches.len());
        }
        Event::FileFailed(err) => println!("  FAILED {}: {}", err.path.display(), err.message),
        _ => {}
    });

    let elapsed = started.elapsed();

    match result {
        Ok(m) => {
            let total_files = m.files_copied + m.files_linked;
            println!("\nsnapshot {}", m.id);
            println!("  outcome        : {:?}", m.outcome);
            println!("  files copied   : {}", m.files_copied);
            println!("  files linked   : {}", m.files_linked);
            println!("  files failed   : {}", m.files_failed);
            println!("  bytes copied   : {}", human(m.bytes_copied));
            println!("  bytes linked   : {} (not written)", human(m.bytes_linked));
            println!("  elapsed        : {:.2}s", elapsed.as_secs_f64());
            if elapsed.as_secs_f64() > 0.0 {
                println!(
                    "  throughput     : {}/s, {:.0} files/s",
                    human((m.bytes_copied as f64 / elapsed.as_secs_f64()) as u64),
                    total_files as f64 / elapsed.as_secs_f64()
                );
            }
            println!("  progress events: {progress_events}");
        }
        Err(e) => {
            eprintln!("run failed: {e}");
            std::process::exit(1);
        }
    }
}
