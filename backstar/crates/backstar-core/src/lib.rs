//! BackStar core: the backup engine, shared by the installed app (`BackStar.exe`)
//! and the portable restore tool (`BackStar-Restore.exe`).
//!
//! Nothing in this crate knows about Tauri, WinForms, or any UI. It emits typed
//! [`events::Event`] values over a channel; whoever is driving decides how to render them.
//!
//! Domain knowledge ported from the PowerShell implementation in `app/` carries its
//! original rationale in comments. Several of those rules exist because a past bug
//! was silent -- the exclusion quoting rule and the fail-closed path guards especially.
//! See `UPGRADE-PLAN.md` at the repo root for the full audit.

pub mod config;
pub mod copy;
pub mod engine;
pub mod error;
pub mod events;
pub mod exclude;
pub mod guards;
pub mod mirror;
mod parallel;
pub mod presets;
pub mod repo;
pub mod restore;
pub mod runlock;
pub mod volume;
pub mod walk;

pub use error::{Error, Result};
pub use runlock::RunLock;
