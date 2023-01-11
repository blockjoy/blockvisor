use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// Unknown method
    #[error("Method `{method}` not found")]
    UnknownMethod { method: String },
    /// Network call method is in config but no host specified
    #[error("No host specified for method `{method}`")]
    NoHostSpecified { method: String },
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

    /// We return this type when the parameters required for a blockchain method call contain
    /// injection-unsafe characters.
    #[error("Unsafe subsitution: {0}")]
    UnsafeSubsitution(char),
}

impl Error {
    pub fn unknown_method(method: impl std::fmt::Display) -> Self {
        Self::UnknownMethod {
            method: method.to_string(),
        }
    }

    pub fn no_host(method: impl std::fmt::Display) -> Self {
        Self::NoHostSpecified {
            method: method.to_string(),
        }
    }

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

    /// Convenience method to construct the UnsafeSubsitution variant.
    pub fn unsafe_sub(c: char) -> Self {
        Self::UnsafeSubsitution(c)
    }
}
