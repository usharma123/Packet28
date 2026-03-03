use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CovyError {
    #[error("Failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("IO error: {0}")]
    IoRaw(#[from] std::io::Error),

    #[error("Failed to parse {format} coverage: {detail}")]
    Parse { format: String, detail: String },

    #[error("XML parse error: {0}")]
    Xml(String),

    #[error("Unknown coverage format for {path} (use --format to specify)")]
    UnknownFormat { path: String },

    #[error("Git is not installed or not found in PATH")]
    GitNotFound,

    #[error("Git error: {0}")]
    Git(String),

    #[error("Coverage file is empty: {path}")]
    EmptyInput { path: String },

    #[error("Config error: {0}")]
    Config(String),

    #[error("Cache error: {0}")]
    Cache(String),

    #[error("Path mapping failed: no match for {0}")]
    PathMapping(String),

    #[error("{0}")]
    Other(String),
}

impl CovyError {
    /// Return a user-friendly hint for this error, if any.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            CovyError::UnknownFormat { .. } => {
                Some("Supported formats: lcov, cobertura, jacoco, gocov, llvm-cov")
            }
            CovyError::GitNotFound => Some("Install git or ensure it is available in your PATH"),
            CovyError::EmptyInput { .. } => {
                Some("Check that your test runner generated coverage output")
            }
            _ => None,
        }
    }
}

impl From<toml::de::Error> for CovyError {
    fn from(e: toml::de::Error) -> Self {
        CovyError::Config(e.to_string())
    }
}
