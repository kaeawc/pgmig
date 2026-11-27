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
            None => Error { code: None, message: with_causes(&e), position: None },
        }
    }
}

/// `e` followed by each cause it does not already print, e.g. `error performing TLS handshake:
/// invalid peer certificate: UnknownIssuer`.
fn with_causes(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

pub type Result<T> = std::result::Result<T, Error>;

/// `127.0.0.0/8`, `::1`, or an IPv4-mapped `::ffff:127.x.y.z`.
pub fn is_loopback(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

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

    /// Whether `conninfo` names only servers on this machine: Unix sockets, `localhost` (or a name
    /// under `.localhost`) and loopback addresses, in `host` and `hostaddr`. No host at all is
    /// local (the default is a local socket or `localhost`).
    pub fn is_local_target(conninfo: &str) -> Result<bool> {
        let (rest, _) = tls::split_conninfo(conninfo)?;
        let config: postgres::Config = rest.parse()?;
        let host_local = |h: &postgres::config::Host| match h {
            postgres::config::Host::Tcp(name) => {
                let name = name.trim_end_matches('.').to_ascii_lowercase();
                name == "localhost"
                    || name.ends_with(".localhost")
                    || name.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok_and(|ip| is_loopback(&ip))
            }
            #[cfg(unix)]
            postgres::config::Host::Unix(_) => true,
        };
        Ok(config.get_hosts().iter().all(host_local) && config.get_hostaddrs().iter().all(is_loopback))
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

#[cfg(test)]
mod tests {
    use super::Conn;

    #[test]
    fn local_targets() {
        for t in [
            "host=localhost user=x",
            "host=127.0.0.1",
            "host=::1",
            "host=/tmp",
            "host=db.localhost",
            "host=LOCALHOST.",
            "user=x",
            "postgres://u@localhost:5433/db",
            "postgres://u@[::1]/db",
            "postgres:///db?host=/var/run/postgresql",
            "host=localhost,127.0.0.2",
            "host=localhost hostaddr=127.0.0.1",
        ] {
            assert!(Conn::is_local_target(t).expect(t), "{t}");
        }
        for t in [
            "host=db.example.com",
            "host=10.0.0.5",
            "postgres://u@prod.internal/db",
            "host=localhost,db.example.com",
            "host=localhost hostaddr=10.0.0.5",
            "host=localhostx",
        ] {
            assert!(!Conn::is_local_target(t).expect(t), "{t}");
        }
    }
}
