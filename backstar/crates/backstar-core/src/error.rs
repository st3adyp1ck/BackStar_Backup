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

    #[error("{0}")]
    Io2(#[from] std::io::Error),

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
