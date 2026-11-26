//! A synchronous connection to Postgres (or to pgmem, which speaks the same protocol).
//!
//! Queries use the simple protocol, so every value comes back in Postgres' text form: exactly what
//! the inspector compares.
//!
//! TLS follows the conninfo's `sslmode` and `sslrootcert` (the `tls` module lists the modes), on rustls.

use std::fmt;

mod tls;

#[derive(Debug, thiserror::Error)]
pub struct Error {
    /// SQLSTATE, when the server reported one.
    pub code: Option<String>,
    pub message: String,
    /// The 1-based character position in the query the server reported the error at, if any.
    pub position: Option<u32>,
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
            Some(db) => Error {
                code: Some(db.code().code().to_owned()),
                message: db.message().to_owned(),
                position: match db.position() {
                    Some(postgres::error::ErrorPosition::Original(n)) => Some(*n),
                    _ => None,
                },
            },
            None => Error { code: None, message: e.to_string(), position: None },
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
    /// `sslmode` (default `prefer`) and `sslrootcert` choose TLS and how the server is verified.
    pub fn connect(conninfo: &str) -> Result<Conn> {
        let (rest, tls) = tls::split_conninfo(conninfo)?;
        let mut config: postgres::Config = rest.parse()?;
        let tls = tls.with_parsed(config.get_ssl_mode())?;
        if tls.explicit {
            config.ssl_mode(tls.postgres_mode());
        }
        tls::hosts_from_hostaddrs(&mut config);
        let client = match tls.client_config()? {
            Some(rustls) => {
                let make = tls::MakeRustls::new(rustls);
                match config.connect(make.clone()) {
                    Ok(client) => client,
                    Err(e) if tls::retry_without_tls(tls.mode, make.handshake_failed(), e.as_db_error().is_some()) => {
                        config.ssl_mode(postgres::config::SslMode::Disable);
                        config.connect(postgres::NoTls)?
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            None => config.connect(postgres::NoTls)?,
        };
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
