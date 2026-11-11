use std::path::PathBuf;

/// Everything that can go wrong in the core library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    /// A database error, with the place in Eudora's code where it happened (2026-10-25: "invalid
    /// utf-8 sequence … at index 1" stopped every check, and nothing said which query read it).
    #[error("database: {source} (at {}:{})", .at.file(), .at.line())]
    Sqlite { source: rusqlite::Error, at: &'static std::panic::Location<'static> },
    #[error("search index: {0}")]
    Index(#[from] tantivy::TantivyError),
    #[error("search query: {0}")]
    Query(#[from] tantivy::query::QueryParserError),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the library at {0} is in use by another copy of Eudora")]
    Locked(PathBuf),
    #[error("{0} is not an Eudora library")]
    NotALibrary(PathBuf),
    #[error("damaged data in {pack}: {detail}")]
    Corrupt { pack: String, detail: String },
    #[error("canceled")]
    Canceled,
    /// The network is down or the connection broke (no answer from the server at all):
    /// worth waiting and trying again, unlike an error the server answered with.
    #[error("network: {0}")]
    Network(String),
    /// The server and Eudora couldn't agree on encryption at the level allowed (TLS version
    /// or ciphers): a lower level may work, if the account allows it.
    #[error("{0}")]
    Tls(String),
    /// An import's address already has an account: the user decides whether to add this
    /// archive to it or keep it apart.
    #[error("{address} already has an account ({messages} messages, from {came_from}): add this archive to it, or keep it as an account of its own?")]
    AccountExists { address: String, messages: u64, came_from: String },
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Adds a description to an I/O error ("reading pack 3: …").
pub trait IoContext<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io { context: what(), source })
    }
}

impl From<rusqlite::Error> for Error {
    /// (`?` passes its own place in the code: the error says where it happened)
    #[track_caller]
    fn from(source: rusqlite::Error) -> Self {
        Error::Sqlite { source, at: std::panic::Location::caller() }
    }
}
