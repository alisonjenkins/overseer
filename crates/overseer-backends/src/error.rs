#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("profile {profile:?} cannot build a backend: missing {field}")]
    InvalidProfile {
        profile: String,
        field: &'static str,
    },

    #[error("failed to build the HTTP client for profile {profile:?}")]
    BuildHttpClient {
        profile: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("working directory {path} does not exist")]
    MissingWorkdir { path: std::path::PathBuf },

    #[error("mock backend has no scripted outcome left for this call")]
    MockExhausted,

    #[error("scripted backend error: {message}")]
    Scripted { message: String },
}
