//! A synchronous connection to Postgres (or to pgmem, which speaks the same protocol).
//!
//! Queries use the simple protocol, so every value comes back in Postgres' text form: exactly what
//! the inspector compares.

use std::fmt;

#[derive(Debug, thiserror::Error)]
pub struct Error {
    /// SQLSTATE, when the server reported one.
    pub code: Option<String>,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{} (SQLSTATE {code})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl From<postgres::Error> for Error {
    fn from(e: postgres::Error) -> Error {
        match e.as_db_error() {
            Some(db) => Error { code: Some(db.code().code().to_owned()), message: db.message().to_owned() },
            None => Error { code: None, message: e.to_string() },
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One row, each column in text form; `None` is SQL NULL.
pub type Row = Vec<Option<String>>;

pub struct Conn {
    client: postgres::Client,
}

impl Conn {
    /// `conninfo` is a URL (`postgres://...`) or key/value string (`host=... user=...`).
    pub fn connect(conninfo: &str) -> Result<Conn> {
        let client = postgres::Client::connect(conninfo, postgres::NoTls)?;
        Ok(Conn { client })
    }

    /// Run one or more statements as a single simple-protocol batch (one implicit transaction).
    pub fn batch(&mut self, sql: &str) -> Result<()> {
        self.client.batch_execute(sql)?;
        Ok(())
    }

    /// Run a query and return its rows as text.
    pub fn query(&mut self, sql: &str) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for msg in self.client.simple_query(sql)? {
            if let postgres::SimpleQueryMessage::Row(r) = msg {
                rows.push((0..r.len()).map(|i| r.get(i).map(str::to_owned)).collect());
            }
        }
        Ok(rows)
    }
}
