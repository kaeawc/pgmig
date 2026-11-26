//! TLS for connections: `sslmode` and `sslrootcert` read from the conninfo, rustls on the `ring`
//! provider.
//!
//! The `postgres` crate only knows `sslmode=disable|prefer|require` and rejects keys it does not
//! know, so both settings are taken out of the conninfo here and the rest is handed on unchanged.
//!
//! | `sslmode`     | TLS                          | Certificate check          |
//! |---------------|------------------------------|----------------------------|
//! | `disable`     | never                        | -                          |
//! | `allow`       | as `prefer`                  | as `prefer`                |
//! | `prefer`      | if the server offers it      | none, or chain with a root |
//! | `require`     | always                       | none, or chain with a root |
//! | `verify-ca`   | always                       | chain                      |
//! | `verify-full` | always                       | chain and host name        |
//!
//! "With a root" means an explicit `sslrootcert=<file>`: as in libpq, a root certificate makes
//! `prefer` and `require` verify the chain. Without `sslrootcert`, the chain is checked against
//! the bundled Mozilla roots (`webpki-roots`); `sslrootcert=system` asks for those explicitly and,
//! as in libpq, defaults to and requires `verify-full`. The default `sslmode` is `prefer`; as in
//! libpq, `prefer` retries in plaintext when the TLS handshake fails, and a host given only by
//! `hostaddr` is verified by that IP address.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    fn parse(value: &str) -> Result<SslMode, Error> {
        Ok(match value {
            "disable" => SslMode::Disable,
            // libpq's `allow` tries plaintext first; the `postgres` crate cannot, so it upgrades to
            // `prefer`, which still connects to every server `allow` would.
            "allow" | "prefer" => SslMode::Prefer,
            "require" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" => SslMode::VerifyFull,
            other => return Err(config_error(format!("invalid sslmode value: \"{other}\""))),
        })
    }

    fn postgres(self) -> postgres::config::SslMode {
        match self {
            SslMode::Disable => postgres::config::SslMode::Disable,
            SslMode::Prefer => postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => postgres::config::SslMode::Require,
        }
    }
}

/// Where the trusted roots come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Roots {
    /// No `sslrootcert`: the bundled Mozilla roots.
    Bundled,
    /// `sslrootcert=system`: the bundled Mozilla roots, `verify-full` only.
    System,
    /// `sslrootcert=<path>`: every certificate in a PEM file.
    File(PathBuf),
}

/// How the server certificate is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verification {
    /// Any certificate is accepted (the handshake signature is still checked).
    AcceptAnyCertificate,
    /// The chain must lead to a trusted root; the host name is not checked.
    Chain,
    /// The chain must lead to a trusted root and the certificate must name the host.
    ChainAndHostName,
}

/// The TLS settings taken out of a conninfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TlsOptions {
    pub mode: SslMode,
    pub roots: Roots,
    /// The mode came from the conninfo (or from `sslrootcert=system`). Otherwise it is the
    /// default, and whatever the `postgres` crate parsed from the rest decides (`with_parsed`).
    pub explicit: bool,
}

impl TlsOptions {
    fn from_values(mode: Option<&str>, root: Option<&str>) -> Result<TlsOptions, Error> {
        let roots = match root {
            None | Some("") => Roots::Bundled,
            Some("system") => Roots::System,
            Some(path) => Roots::File(PathBuf::from(path)),
        };
        let (mode, explicit) = match mode {
            Some(m) => (SslMode::parse(m)?, true),
            None if roots == Roots::System => (SslMode::VerifyFull, true),
            None => (SslMode::Prefer, false),
        };
        if roots == Roots::System && mode != SslMode::VerifyFull && mode != SslMode::Disable {
            return Err(config_error("weak sslmode may not be used with sslrootcert=system (use \"verify-full\")".to_owned()));
        }
        Ok(TlsOptions { mode, roots, explicit })
    }

    /// When the conninfo named no `sslmode` that was taken out, the mode the `postgres` crate
    /// parsed from the rest stands: it is never overridden, and TLS is set up to match it.
    pub(crate) fn with_parsed(self, parsed: postgres::config::SslMode) -> Result<TlsOptions, Error> {
        use postgres::config::SslMode as Pg;
        if self.explicit {
            return Ok(self);
        }
        let mode = match parsed {
            Pg::Disable => SslMode::Disable,
            Pg::Require => SslMode::Require,
            _ => SslMode::Prefer,
        };
        Ok(TlsOptions { mode, ..self })
    }

    /// `None` when TLS is off.
    pub(crate) fn verification(&self) -> Option<Verification> {
        let explicit_root = matches!(self.roots, Roots::File(_));
        Some(match self.mode {
            SslMode::Disable => return None,
            SslMode::Prefer | SslMode::Require if explicit_root => Verification::Chain,
            SslMode::Prefer | SslMode::Require => Verification::AcceptAnyCertificate,
            SslMode::VerifyCa => Verification::Chain,
            SslMode::VerifyFull => Verification::ChainAndHostName,
        })
    }

    pub(crate) fn postgres_mode(&self) -> postgres::config::SslMode {
        self.mode.postgres()
    }

    /// The rustls configuration, or `None` when TLS is off.
    pub(crate) fn client_config(&self) -> Result<Option<ClientConfig>, Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let Some(verifier) = self.verifier(provider.clone())? else { return Ok(None) };
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| config_error(format!("TLS configuration: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        Ok(Some(config))
    }

    fn verifier(&self, provider: Arc<CryptoProvider>) -> Result<Option<Arc<dyn ServerCertVerifier>>, Error> {
        let Some(verification) = self.verification() else { return Ok(None) };
        Ok(Some(match verification {
            Verification::AcceptAnyCertificate => {
                Arc::new(AcceptAnyServerCertificate { algorithms: provider.signature_verification_algorithms })
            }
            Verification::Chain => Arc::new(VerifyChainOnly { inner: self.webpki_verifier(provider)? }),
            Verification::ChainAndHostName => self.webpki_verifier(provider)?,
        }))
    }

    fn webpki_verifier(&self, provider: Arc<CryptoProvider>) -> Result<Arc<WebPkiServerVerifier>, Error> {
        let mut roots = RootCertStore::empty();
        match &self.roots {
            Roots::Bundled | Roots::System => {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            Roots::File(path) => {
                let read_error =
                    |e: &dyn std::fmt::Display| config_error(format!("could not read root certificate file \"{}\": {e}", path.display()));
                for cert in CertificateDer::pem_file_iter(path).map_err(|e| read_error(&e))? {
                    let cert = cert.map_err(|e| read_error(&e))?;
                    roots.add(cert).map_err(|e| read_error(&e))?;
                }
                if roots.is_empty() {
                    return Err(read_error(&"no certificates found"));
                }
            }
        }
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .map_err(|e| config_error(format!("TLS configuration: {e}")))
    }
}

fn config_error(message: String) -> Error {
    Error { code: None, message, position: None }
}

/// Takes `sslmode` and `sslrootcert` out of a conninfo (URL or key/value form). Returns the rest
/// for the `postgres` crate and the TLS settings. A conninfo that does not tokenize is returned as
/// is, so the `postgres` crate reports the syntax error.
pub(crate) fn split_conninfo(conninfo: &str) -> Result<(String, TlsOptions), Error> {
    let split = if conninfo.starts_with("postgres://") || conninfo.starts_with("postgresql://") {
        split_url(conninfo)
    } else {
        split_key_value(conninfo)
    };
    let (rest, mode, root) = split.unwrap_or_else(|| (conninfo.to_owned(), None, None));
    let options = TlsOptions::from_values(mode.as_deref(), root.as_deref())?;
    Ok((rest, options))
}

type Split = (String, Option<String>, Option<String>);

const SSL_KEYS: [&str; 2] = ["sslmode", "sslrootcert"];

/// Mirrors the `postgres` crate's URL parser, `scheme://[userinfo@][hosts][/dbname][?params]`:
/// the user info ends at the first `@`, the hosts at the first `/` or `?`, the database name at
/// the next `?`. Only what that parser reads as parameters is looked at, so a `?` (or `#`, `/`)
/// in the user info never starts the query.
fn split_url(url: &str) -> Option<Split> {
    let scheme_end = url.find("://")? + 3;
    let after_userinfo = url[scheme_end..].find('@').map_or(scheme_end, |at| scheme_end + at + 1);
    let query_start = match url[after_userinfo..].find(['/', '?']).map(|i| after_userinfo + i) {
        None => None,
        Some(i) if url.as_bytes()[i] == b'?' => Some(i),
        Some(slash) => url[slash..].find('?').map(|i| slash + i),
    };
    let Some(query_start) = query_start else {
        return Some((url.to_owned(), None, None));
    };
    let (base, mut query) = (&url[..query_start], &url[query_start + 1..]);
    let (mut mode, mut root) = (None, None);
    let mut kept = Vec::new();
    // As the `postgres` crate: the key runs to the next `=`, the value to the next `&`.
    while !query.is_empty() {
        let eq = query.find('=')?;
        let end = query[eq + 1..].find('&').map_or(query.len(), |i| eq + 1 + i);
        let (param, key, value) = (&query[..end], &query[..eq], &query[eq + 1..end]);
        query = query.get(end + 1..).unwrap_or("");
        match percent_decode(key)?.as_str() {
            "sslmode" => mode = Some(percent_decode(value)?),
            "sslrootcert" => root = Some(percent_decode(value)?),
            _ => kept.push(param),
        }
    }
    let rest = if kept.is_empty() { base.to_owned() } else { format!("{base}?{}", kept.join("&")) };
    Some((rest, mode, root))
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let hex = |b: u8| (b as char).to_digit(16).and_then(|d| u8::try_from(d).ok());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Mirrors the `postgres` crate's key/value tokenizer: `key = value`, values bare or in single
/// quotes, backslash escapes in both.
fn split_key_value(s: &str) -> Option<Split> {
    let (mut mode, mut root) = (None, None);
    let mut rest = String::with_capacity(s.len());
    let mut copied_to = 0;
    let mut it = s.char_indices().peekable();
    let skip_ws = |it: &mut std::iter::Peekable<std::str::CharIndices<'_>>| {
        while it.next_if(|&(_, c)| c.is_whitespace()).is_some() {}
    };
    loop {
        skip_ws(&mut it);
        let Some(&(start, _)) = it.peek() else { break };
        while it.next_if(|&(_, c)| !c.is_whitespace() && c != '=').is_some() {}
        let key_end = it.peek().map_or(s.len(), |&(i, _)| i);
        let key = &s[start..key_end];
        skip_ws(&mut it);
        it.next_if(|&(_, c)| c == '=')?;
        skip_ws(&mut it);
        let mut value = String::new();
        if it.next_if(|&(_, c)| c == '\'').is_some() {
            loop {
                match it.next()? {
                    (_, '\'') => break,
                    (_, '\\') => value.extend(it.next().map(|(_, c)| c)),
                    (_, c) => value.push(c),
                }
            }
        } else {
            while let Some((_, c)) = it.next_if(|&(_, c)| !c.is_whitespace()) {
                if c == '\\' {
                    value.extend(it.next().map(|(_, c)| c));
                } else {
                    value.push(c);
                }
            }
            if value.is_empty() {
                return None;
            }
        }
        let end = it.peek().map_or(s.len(), |&(i, _)| i);
        if SSL_KEYS.contains(&key) {
            rest.push_str(&s[copied_to..start]);
            copied_to = end;
            if key == "sslmode" {
                mode = Some(value);
            } else {
                root = Some(value);
            }
        }
    }
    rest.push_str(&s[copied_to..]);
    Some((rest, mode, root))
}

/// As libpq: a host given only by `hostaddr` is verified (and named in SNI) by that address. The
/// `postgres` crate takes the TLS server name from `host` alone, so each `hostaddr` is also given
/// as the `host`; the connection still goes to the `hostaddr`, and an IP server name means
/// `verify-full` checks the certificate's IP address SAN.
pub(crate) fn hosts_from_hostaddrs(config: &mut postgres::Config) {
    if config.get_hosts().is_empty() {
        for addr in config.get_hostaddrs().to_vec() {
            config.host(&addr.to_string());
        }
    }
}

/// As libpq, `prefer` (only) falls back to plaintext when the TLS handshake fails; a server error
/// (authentication, unknown database, ...) is not retried, and stricter modes never fall back.
pub(crate) fn retry_without_tls(mode: SslMode, handshake_failed: bool, server_error: bool) -> bool {
    mode == SslMode::Prefer && handshake_failed && !server_error
}

/// `sslmode=require` (and `prefer`) without a root certificate: encrypt, but trust whatever
/// certificate the server presents. The handshake signature is still checked, so the server must
/// hold the key of the certificate it sent.
#[derive(Debug)]
struct AcceptAnyServerCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyServerCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// `sslmode=verify-ca`: the full WebPKI check, except that a certificate for another name passes.
#[derive(Debug)]
struct VerifyChainOnly {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for VerifyChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Err(rustls::Error::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. })) => {
                Ok(ServerCertVerified::assertion())
            }
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The `postgres` crate's TLS hook, backed by rustls.
#[derive(Clone)]
pub(crate) struct MakeRustls {
    config: Arc<ClientConfig>,
    handshake_failed: Arc<AtomicBool>,
}

impl MakeRustls {
    pub(crate) fn new(config: ClientConfig) -> MakeRustls {
        MakeRustls { config: Arc::new(config), handshake_failed: Arc::new(AtomicBool::new(false)) }
    }

    /// Whether a TLS handshake started by this connector failed.
    pub(crate) fn handshake_failed(&self) -> bool {
        self.handshake_failed.load(Ordering::Relaxed)
    }
}

impl<S> MakeTlsConnect<S> for MakeRustls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> Result<RustlsConnect, io::Error> {
        Ok(RustlsConnect {
            connector: tokio_rustls::TlsConnector::from(self.config.clone()),
            domain: domain.to_owned(),
            failed: self.handshake_failed.clone(),
        })
    }
}

pub(crate) struct RustlsConnect {
    connector: tokio_rustls::TlsConnector,
    domain: String,
    failed: Arc<AtomicBool>,
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = RustlsConnectFuture<S>;

    fn connect(self, stream: S) -> RustlsConnectFuture<S> {
        // The name is only parsed here: Unix-socket connections never reach the handshake.
        match ServerName::try_from(self.domain) {
            Ok(name) => RustlsConnectFuture::Connect(Box::new(self.connector.connect(name, stream)), self.failed),
            Err(e) => {
                self.failed.store(true, Ordering::Relaxed);
                RustlsConnectFuture::Failed(Some(io::Error::new(io::ErrorKind::InvalidInput, e)))
            }
        }
    }
}

pub(crate) enum RustlsConnectFuture<S> {
    Connect(Box<tokio_rustls::Connect<S>>, Arc<AtomicBool>),
    Failed(Option<io::Error>),
}

impl<S> Future for RustlsConnectFuture<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Output = io::Result<RustlsStream<S>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            RustlsConnectFuture::Connect(connect, failed) => match Pin::new(connect.as_mut()).poll(cx) {
                Poll::Ready(Err(e)) => {
                    failed.store(true, Ordering::Relaxed);
                    Poll::Ready(Err(e))
                }
                other => other.map_ok(|s| RustlsStream(Box::new(s))),
            },
            RustlsConnectFuture::Failed(error) => {
                Poll::Ready(Err(error.take().unwrap_or_else(|| io::Error::other("TLS connect polled after completion"))))
            }
        }
    }
}

pub(crate) struct RustlsStream<S>(Box<tokio_rustls::client::TlsStream<S>>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for RustlsStream<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for RustlsStream<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream for RustlsStream<S> {
    /// No `tls-server-end-point` binding: SCRAM runs without channel binding over TLS
    /// (`channel_binding=require` is refused).
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> (String, TlsOptions) {
        split_conninfo(s).expect("split")
    }

    fn opts(mode: SslMode, roots: Roots) -> TlsOptions {
        TlsOptions { mode, roots, explicit: true }
    }

    fn default_opts() -> TlsOptions {
        TlsOptions { mode: SslMode::Prefer, roots: Roots::Bundled, explicit: false }
    }

    #[test]
    fn key_value_without_ssl_keys_is_unchanged() {
        let s = "host=localhost port=5432 user=postgres";
        assert_eq!(split(s), (s.to_owned(), default_opts()));
    }

    #[test]
    fn key_value_ssl_keys_are_taken_out() {
        let (rest, o) = split("host=db sslmode=verify-full sslrootcert=/etc/ca.pem user=me");
        assert_eq!(rest, "host=db   user=me");
        assert_eq!(o, opts(SslMode::VerifyFull, Roots::File("/etc/ca.pem".into())));
    }

    #[test]
    fn key_value_quoted_spaced_and_escaped() {
        let (rest, o) = split("host=db sslmode = 'verify-ca' sslrootcert='/my certs/it\\'s.pem' password='a b'");
        assert_eq!(rest, "host=db   password='a b'");
        assert_eq!(o, opts(SslMode::VerifyCa, Roots::File("/my certs/it's.pem".into())));
    }

    #[test]
    fn key_value_last_value_wins() {
        let (_, o) = split("sslmode=disable sslmode=require");
        assert_eq!(o.mode, SslMode::Require);
    }

    #[test]
    fn malformed_key_value_is_passed_through() {
        let s = "host=db sslmode";
        assert_eq!(split(s).0, s);
    }

    #[test]
    fn url_without_query_is_unchanged() {
        let s = "postgres://me@db:5432/app";
        assert_eq!(split(s), (s.to_owned(), default_opts()));
    }

    #[test]
    fn url_ssl_params_are_taken_out() {
        let (rest, o) =
            split("postgresql://me@db/app?application_name=x&sslmode=verify-full&sslrootcert=%2Ftmp%2Fca%20x.pem&connect_timeout=5");
        assert_eq!(rest, "postgresql://me@db/app?application_name=x&connect_timeout=5");
        assert_eq!(o, opts(SslMode::VerifyFull, Roots::File("/tmp/ca x.pem".into())));
    }

    #[test]
    fn url_with_only_ssl_params_drops_the_query() {
        let (rest, o) = split("postgres://db/app?sslmode=require");
        assert_eq!(rest, "postgres://db/app");
        assert_eq!(o, opts(SslMode::Require, Roots::Bundled));
    }

    #[test]
    fn rest_parses_with_the_postgres_crate() {
        for s in ["host=db sslmode=verify-full sslrootcert=/ca.pem user=me", "postgres://me@db/app?sslmode=verify-ca&sslrootcert=/ca.pem"] {
            let (rest, _) = split(s);
            rest.parse::<postgres::Config>().expect("postgres accepts the rest");
        }
    }

    #[test]
    fn modes() {
        for (value, mode) in [
            ("disable", SslMode::Disable),
            ("allow", SslMode::Prefer),
            ("prefer", SslMode::Prefer),
            ("require", SslMode::Require),
            ("verify-ca", SslMode::VerifyCa),
            ("verify-full", SslMode::VerifyFull),
        ] {
            assert_eq!(split(&format!("sslmode={value}")).1.mode, mode, "{value}");
        }
        let err = split_conninfo("sslmode=bogus").expect_err("bogus sslmode");
        assert_eq!(err.message, "invalid sslmode value: \"bogus\"");
    }

    #[test]
    fn sslrootcert_system() {
        assert_eq!(split("sslrootcert=system").1, opts(SslMode::VerifyFull, Roots::System));
        assert!(split_conninfo("sslrootcert=system sslmode=require").is_err());
        assert!(split_conninfo("sslrootcert=system sslmode=verify-full").is_ok());
    }

    #[test]
    fn verifier_selection() {
        use Verification::*;
        let file = || Roots::File("/ca.pem".into());
        let cases = [
            (opts(SslMode::Disable, Roots::Bundled), None),
            (opts(SslMode::Disable, file()), None),
            (opts(SslMode::Prefer, Roots::Bundled), Some(AcceptAnyCertificate)),
            (opts(SslMode::Require, Roots::Bundled), Some(AcceptAnyCertificate)),
            (opts(SslMode::Prefer, file()), Some(Chain)),
            (opts(SslMode::Require, file()), Some(Chain)),
            (opts(SslMode::VerifyCa, Roots::Bundled), Some(Chain)),
            (opts(SslMode::VerifyCa, file()), Some(Chain)),
            (opts(SslMode::VerifyFull, Roots::Bundled), Some(ChainAndHostName)),
            (opts(SslMode::VerifyFull, Roots::System), Some(ChainAndHostName)),
            (opts(SslMode::VerifyFull, file()), Some(ChainAndHostName)),
        ];
        for (o, want) in cases {
            assert_eq!(o.verification(), want, "{o:?}");
        }
    }

    #[test]
    fn postgres_modes() {
        use postgres::config::SslMode as Pg;
        assert_eq!(opts(SslMode::Disable, Roots::Bundled).postgres_mode(), Pg::Disable);
        assert_eq!(opts(SslMode::Prefer, Roots::Bundled).postgres_mode(), Pg::Prefer);
        for m in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            assert_eq!(opts(m, Roots::Bundled).postgres_mode(), Pg::Require);
        }
    }

    #[test]
    fn client_configs_build() {
        assert!(opts(SslMode::Disable, Roots::Bundled).client_config().expect("ok").is_none());
        for m in [SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            assert!(opts(m, Roots::Bundled).client_config().expect("config").is_some());
        }
    }

    #[test]
    fn missing_root_file_is_an_error() {
        let err = opts(SslMode::VerifyFull, Roots::File("/nonexistent/ca.pem".into())).client_config().expect_err("missing file");
        assert!(err.message.starts_with("could not read root certificate file"), "{}", err.message);
    }

    #[test]
    fn url_userinfo_never_starts_the_query() {
        let (rest, o) = split("postgres://u:p?w@db/app?sslmode=require");
        assert_eq!(rest, "postgres://u:p?w@db/app");
        assert_eq!(o, opts(SslMode::Require, Roots::Bundled));
        let config: postgres::Config = rest.parse().expect("rest parses");
        assert_eq!(config.get_password(), Some(&b"p?w"[..]));
    }

    #[test]
    fn url_password_special_characters() {
        let (rest, o) = split("postgres://u:a%40b#c/d?e&f=g@db:5432/app?sslmode=verify-ca&application_name=x");
        assert_eq!(rest, "postgres://u:a%40b#c/d?e&f=g@db:5432/app?application_name=x");
        assert_eq!(o, opts(SslMode::VerifyCa, Roots::Bundled));
        let config: postgres::Config = rest.parse().expect("rest parses");
        assert_eq!(config.get_password(), Some(&b"a@b#c/d?e&f=g"[..]));
        assert_eq!(config.get_dbname(), Some("app"));
        assert_eq!(config.get_application_name(), Some("x"));
    }

    #[test]
    fn url_query_without_path_and_ipv6_host() {
        let (rest, o) = split("postgres://u@[::1]:5432?sslmode=disable");
        assert_eq!(rest, "postgres://u@[::1]:5432");
        assert_eq!(o, opts(SslMode::Disable, Roots::Bundled));
    }

    #[test]
    fn url_ssl_key_in_userinfo_is_not_a_parameter() {
        let (rest, o) = split("postgres://u:x?sslmode=disable@db/app");
        assert_eq!(rest, "postgres://u:x?sslmode=disable@db/app");
        assert_eq!(o, default_opts());
    }

    /// The mode the `postgres` crate ends up with after `Conn::connect` handled the conninfo.
    fn effective_mode(s: &str) -> postgres::config::SslMode {
        let (rest, tls) = split(s);
        let config: postgres::Config = rest.parse().expect("rest parses");
        let tls = tls.with_parsed(config.get_ssl_mode()).expect("with_parsed");
        if tls.explicit { tls.postgres_mode() } else { config.get_ssl_mode() }
    }

    fn strength(m: postgres::config::SslMode) -> u8 {
        use postgres::config::SslMode as Pg;
        match m {
            Pg::Disable => 0,
            Pg::Prefer => 1,
            _ => 2,
        }
    }

    #[test]
    fn the_mode_the_postgres_crate_parses_is_never_weakened() {
        let passwords = ["p", "p?w", "p#w", "p/w", "p%40w", "p%3Fsslmode%3Ddisable", "p?sslmode=disable", "a/b?c#d"];
        let mut inputs = Vec::new();
        for pw in passwords {
            for mode in ["disable", "prefer", "require"] {
                inputs.push(format!("postgres://u:{pw}@db/app?sslmode={mode}"));
                inputs.push(format!("postgresql://u:{pw}@db:5432,db2:5433/app?application_name=x&sslmode={mode}&connect_timeout=5"));
                inputs.push(format!("postgres://u:{pw}@db?sslmode={mode}"));
                inputs.push(format!("postgres://u:{pw}@db/app?a=1&&sslmode={mode}"));
                inputs.push(format!("postgres://u:{pw}@db/app?sslmode=require&sslmode={mode}"));
                inputs.push(format!("host=db password='{pw}' sslmode={mode}"));
            }
            inputs.push(format!("postgres://u:{pw}@db/app"));
        }
        let mut checked = 0;
        for s in &inputs {
            // Only what the crate accepts as is has a mode to compare with.
            let Ok(original) = s.parse::<postgres::Config>() else { continue };
            let effective = effective_mode(s);
            assert!(strength(effective) >= strength(original.get_ssl_mode()), "{s}: {:?} < {:?}", effective, original.get_ssl_mode());
            assert_eq!(effective, original.get_ssl_mode(), "{s}");
            checked += 1;
        }
        assert!(checked > inputs.len() / 2, "{checked} of {}", inputs.len());
    }

    #[test]
    fn with_parsed_follows_the_crate_only_without_an_explicit_mode() {
        use postgres::config::SslMode as Pg;
        let o = default_opts().with_parsed(Pg::Require).expect("ok");
        assert_eq!(o.mode, SslMode::Require);
        assert_eq!(default_opts().with_parsed(Pg::Disable).expect("ok").mode, SslMode::Disable);
        assert_eq!(default_opts().with_parsed(Pg::Prefer).expect("ok"), default_opts());
        let explicit = opts(SslMode::VerifyFull, Roots::Bundled);
        assert_eq!(explicit.clone().with_parsed(Pg::Disable).expect("ok"), explicit);
        assert!(split("sslrootcert=system").1.explicit);
    }

    fn tls_server_names(conninfo: &str) -> Vec<ServerName<'static>> {
        let mut config: postgres::Config = conninfo.parse().expect("parses");
        hosts_from_hostaddrs(&mut config);
        config
            .get_hosts()
            .iter()
            .map(|h| match h {
                postgres::config::Host::Tcp(name) => ServerName::try_from(name.clone()).expect("server name"),
                #[cfg(unix)]
                postgres::config::Host::Unix(_) => panic!("unix socket"),
            })
            .collect()
    }

    #[test]
    fn tls_server_name_selection() {
        use std::net::IpAddr;
        let ip = |s: &str| ServerName::IpAddress(s.parse::<IpAddr>().expect("ip").into());
        assert_eq!(tls_server_names("hostaddr=127.0.0.1 user=me"), vec![ip("127.0.0.1")]);
        assert_eq!(tls_server_names("hostaddr=::1,10.0.0.2 user=me"), vec![ip("::1"), ip("10.0.0.2")]);
        assert_eq!(tls_server_names("host=db.example hostaddr=10.0.0.1 user=me"), vec![ServerName::try_from("db.example").expect("dns")]);
        assert_eq!(tls_server_names("host=10.0.0.3 user=me"), vec![ip("10.0.0.3")]);
        let mut config: postgres::Config = "hostaddr=10.0.0.1 user=me".parse().expect("parses");
        hosts_from_hostaddrs(&mut config);
        assert_eq!(config.get_hostaddrs(), ["10.0.0.1".parse::<IpAddr>().expect("ip")], "the connection still goes to hostaddr");
    }

    #[test]
    fn only_prefer_retries_without_tls_after_a_failed_handshake() {
        for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            for handshake_failed in [false, true] {
                for server_error in [false, true] {
                    let want = mode == SslMode::Prefer && handshake_failed && !server_error;
                    assert_eq!(retry_without_tls(mode, handshake_failed, server_error), want, "{mode:?} {handshake_failed} {server_error}");
                }
            }
        }
    }

    #[test]
    fn handshake_failure_is_recorded() {
        let make = MakeRustls::new(opts(SslMode::Require, Roots::Bundled).client_config().expect("ok").expect("tls"));
        assert!(!make.handshake_failed());
        let mut m = make.clone();
        let connect = <MakeRustls as MakeTlsConnect<tokio::io::DuplexStream>>::make_tls_connect(&mut m, "").expect("connector");
        let (client, _server) = tokio::io::duplex(64);
        drop(TlsConnect::connect(connect, client));
        assert!(make.handshake_failed(), "an unusable server name fails the handshake");
    }
}
