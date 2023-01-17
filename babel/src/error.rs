use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// Error while running sh command
    #[error("Failed to run command `{args}`, got output `{output}`")]
    Command { args: String, output: String },
    /// Keys management error
    #[error("Keys management error: {output}")]
    Keys { output: String },

    /// Reqwest error
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),

    /// JSON error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn command(args: impl std::fmt::Display, out: impl std::fmt::Debug) -> Self {
        Self::Command {
            args: args.to_string(),
            output: format!("{out:?}"),
        }
    }

    pub fn keys(out: impl std::fmt::Display) -> Self {
        Self::Keys {
            output: format!("{out}"),
        }
    }
}
