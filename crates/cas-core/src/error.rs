use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CasError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("could not determine the user home directory")]
    HomeUnavailable,
    #[error("Codex credential storage mode '{0}' is not supported; CAS v1 requires file storage")]
    UnsupportedCredentialStore(String),
    #[error("no active Codex credential file found at {0}")]
    ActiveAuthMissing(PathBuf),
    #[error("saved credential file is missing for account {0}")]
    SavedAuthMissing(String),
    #[error("invalid Codex credential file: {0}")]
    InvalidAuth(String),
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("account selector is ambiguous: {selector}; matches: {matches}")]
    AmbiguousAccount { selector: String, matches: String },
    #[error("cannot remove the active account; switch away from it first")]
    ActiveAccountRemoval,
    #[error("another CAS operation is already modifying account state")]
    LockBusy,
    #[error("remote Codex service error: {0}")]
    Remote(String),
    #[error("failed to terminate all Codex processes: {0:?}")]
    CodexStillRunning(Vec<crate::ProcessInfo>),
    #[error("credential verification failed: {0}")]
    Verification(String),
}

pub type Result<T> = std::result::Result<T, CasError>;
