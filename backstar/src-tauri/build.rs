use std::path::Path;

fn main() {
    ensure_restore_tool_resource_exists();
    tauri_build::build()
}

/// `tauri.conf.json`'s `bundle.resources` names `../target/release/BackStar-Restore.exe` so
/// the NSIS installer ships it next to `BackStar.exe`. Tauri's own build script copies that
/// resource into the target directory on *every* build of this crate -- not just `tauri
/// build` -- and fails outright if the source file is missing. A real release build always
/// has it (`beforeBuildCommand` builds `backstar-restore --release` first), but a plain
/// `cargo build`/`cargo test` of just this crate has no reason to have produced a release
/// binary of a sibling workspace member.
///
/// So: if the release binary is missing, fall back to whatever debug binary the workspace
/// already built (a normal `cargo build --workspace` produces one for free). This is purely
/// to satisfy that existence check for local dev/test builds, which never bundle anything --
/// a real `tauri build` always overwrites it with the genuine optimized binary before this
/// runs, since `beforeBuildCommand` completes before the Rust build starts.
fn ensure_restore_tool_resource_exists() {
    let release_path = Path::new("../target/release/BackStar-Restore.exe");
    if release_path.exists() {
        return;
    }

    let debug_path = Path::new("../target/debug/BackStar-Restore.exe");
    if !debug_path.exists() {
        // Neither exists yet -- a genuinely fresh checkout that has never run `cargo build
        // --workspace`. Leave it be; `tauri_build::build()` will report the missing
        // resource with a clearer message than anything duplicated here.
        return;
    }

    if let Some(parent) = release_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(debug_path, release_path);
}
