use std::fmt;

/// Errors surfaced to the CLI; every message here is free of credentials by construction.
#[derive(Debug)]
pub enum Error {
    Io {
        context: String,
        source: std::io::Error,
    },
    Invalid(String),
    Keystore(crate::keystore::KeystoreError),
}

impl Error {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Invalid(msg) => f.write_str(msg),
            Error::Keystore(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::keystore::KeystoreError> for Error {
    fn from(e: crate::keystore::KeystoreError) -> Self {
        Error::Keystore(e)
    }
}
