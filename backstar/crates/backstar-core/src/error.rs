use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A path could not be canonicalised, so no containment guarantee can be made
    /// about it. Callers MUST treat this as unsafe and refuse the operation --
    /// see `guards::paths_overlap`.
    #[error("path could not be resolved: {0}")]
    Unresolvable(PathBuf),

    #[error("destination {dest} overlaps source {src}")]
    Overlap { src: PathBuf, dest: PathBuf },

    #[error("not a backstar repository: {0}")]
    NotARepo(PathBuf),

    #[error("unsupported repo schema version {found} (this build understands {understood})")]
    RepoSchema { found: u32, understood: u32 },

    /// A source folder that could not be listed at all. The run must fail rather than
    /// write an empty "Completed" snapshot over a tree that was never read -- that
    /// snapshot would become the link parent every later run trusts.
    #[error("could not read the source folder {0} (access denied, or it vanished mid-scan)")]
    UnreadableSourceRoot(PathBuf),

    /// Part of a source tree could not be read. Mirror jobs refuse outright in this case:
    /// the unreadable entries look exactly like source-side deletions, and a deletion
    /// plan built over that view would permanently destroy real destination files.
    #[error("could not read {count} item(s) under {root} (for example: {samples}) -- \
             refusing to build a deletion plan over a tree that could not be fully read")]
    SourceUnreadable { root: PathBuf, count: u64, samples: String },

    /// A junction or symlink was offered as a ROOT -- of a walk, or of a mirror
    /// destination subfolder. Reparse points are never followed anywhere in the tree,
    /// and the root is not an exception: following it here would read (or, for a mirror,
    /// delete) inside a directory that is not where the path says it is.
    #[error("{0} is a junction or symlink -- refusing to use it as a root. Point at the \
             real folder instead")]
    ReparseRoot(PathBuf),

    /// A mirror destination tree contains a snapshot repository (a `.backstar` folder).
    /// Mirroring onto it would purge backup history, so the run is refused before
    /// anything is deleted.
    #[error("{0} contains a BackStar snapshot repository -- mirroring onto it would \
             delete backup history, not create a backup. Point this job at a different \
             folder")]
    RepoInsideDestination(PathBuf),

    /// The destination changed between the mirror preview and the confirmed run: files
    /// appeared that the preview never showed as deletion candidates. Deleting them
    /// without a fresh confirmation is exactly the silent-destruction shape the preview
    /// exists to prevent, so the run aborts before touching anything.
    #[error("the destination changed since the mirror preview: {count} file(s) would be \
             deleted that the preview did not show (for example: {samples}) -- run the \
             preview again before confirming")]
    DestinationChangedSincePreview { count: u64, samples: String },

    /// D5/F22: a restore under `OverwritePolicy::Fail` found destination files it would
    /// overwrite, so it was refused before anything was written. `newer` counts the
    /// dangerous subset (destination newer than the snapshot copy).
    #[error("restore refused: {count} existing file(s) would be overwritten at {dest} \
             ({newer} of them newer than the snapshot) -- nothing was written; choose an \
             overwrite policy or a different destination")]
    RestoreWouldOverwrite { dest: PathBuf, count: u64, newer: u64 },

    /// Another run -- in this process or a different one -- already holds the run lock
    /// for this destination. Two runs against one destination would interleave snapshot
    /// ids and manifest writes (or, for a mirror, deletions) into a state neither run
    /// planned; the in-process `Runner` mutex in the installed app cannot see the
    /// headless `--run-job` process, so the exclusion lives in a lock file on the
    /// destination itself (see `runlock.rs`). The path is the repository root or mirror
    /// destination that is busy.
    #[error("another BackStar run is already working on {0} -- let it finish (or cancel \
             it) and try again; if you are certain no run is active, delete the lock file \
             (.backstar/run.lock or .backstar-mirror.lock) inside it")]
    RunInProgress(PathBuf),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io { path: path.into(), source }
    }
}
