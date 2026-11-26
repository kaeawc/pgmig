//! TLS against a real server. Skipped unless `PGMIG_TEST_TLS_DSN` is set (CI job `tls`):
//!
//! - `PGMIG_TEST_TLS_DSN`: key/value conninfo without `host` or ssl settings, e.g.
//!   `port=5433 user=postgres`. The server's certificate names `localhost` only.
//! - `PGMIG_TEST_TLS_ROOT`: PEM file of the CA that signed the server certificate.
//! - `PGMIG_TEST_TLS_WRONG_ROOT`: PEM file of an unrelated CA.

use pgmig_conn::Conn;

struct Env {
    dsn: String,
    root: String,
    wrong_root: String,
}

fn env() -> Option<Env> {
    let var = |k| std::env::var(k).ok().filter(|s: &String| !s.is_empty());
    let dsn = var("PGMIG_TEST_TLS_DSN")?;
    Some(Env {
        dsn,
        root: var("PGMIG_TEST_TLS_ROOT").expect("PGMIG_TEST_TLS_ROOT"),
        wrong_root: var("PGMIG_TEST_TLS_WRONG_ROOT").expect("PGMIG_TEST_TLS_WRONG_ROOT"),
    })
}

fn ssl_in_use(conn: &mut Conn) -> String {
    let rows = conn.query("SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()").expect("query pg_stat_ssl");
    rows[0][0].clone().expect("ssl column")
}

#[test]
fn verify_full_with_the_right_root_connects_over_tls() {
    let Some(e) = env() else { return };
    let mut conn =
        Conn::connect(&format!("host=localhost {} sslmode=verify-full sslrootcert={}", e.dsn, e.root)).expect("verify-full connects");
    assert_eq!(ssl_in_use(&mut conn), "true");
}

#[test]
fn verify_full_url_form_connects() {
    let Some(e) = env() else { return };
    let port = e.dsn.split_whitespace().find_map(|kv| kv.strip_prefix("port=")).unwrap_or("5432");
    let mut conn = Conn::connect(&format!("postgres://postgres@localhost:{port}/postgres?sslmode=verify-full&sslrootcert={}", e.root))
        .expect("verify-full (URL) connects");
    assert_eq!(ssl_in_use(&mut conn), "true");
}

#[test]
fn verify_full_with_a_wrong_root_fails() {
    let Some(e) = env() else { return };
    let err = Conn::connect(&format!("host=localhost {} sslmode=verify-full sslrootcert={}", e.dsn, e.wrong_root))
        .err()
        .expect("a certificate from another CA is refused");
    assert!(err.message.contains("certificate"), "{err}");
}

#[test]
fn verify_full_with_the_bundled_roots_fails() {
    let Some(e) = env() else { return };
    assert!(Conn::connect(&format!("host=localhost {} sslmode=verify-full", e.dsn)).is_err());
}

#[test]
fn verify_full_checks_the_host_name_and_verify_ca_does_not() {
    let Some(e) = env() else { return };
    // The certificate names `localhost`, not 127.0.0.1.
    let ip = |mode: &str| Conn::connect(&format!("host=127.0.0.1 {} sslmode={mode} sslrootcert={}", e.dsn, e.root));
    assert!(ip("verify-full").is_err());
    let mut conn = ip("verify-ca").expect("verify-ca ignores the host name");
    assert_eq!(ssl_in_use(&mut conn), "true");
    assert!(Conn::connect(&format!("host=localhost {} sslmode=verify-ca sslrootcert={}", e.dsn, e.wrong_root)).is_err());
}

#[test]
fn require_and_prefer_encrypt_without_verifying() {
    let Some(e) = env() else { return };
    for mode in ["require", "prefer"] {
        let mut conn =
            Conn::connect(&format!("host=127.0.0.1 {} sslmode={mode}", e.dsn)).unwrap_or_else(|err| panic!("sslmode={mode}: {err}"));
        assert_eq!(ssl_in_use(&mut conn), "true", "sslmode={mode}");
    }
}

#[test]
fn disable_connects_in_plaintext() {
    let Some(e) = env() else { return };
    let mut conn = Conn::connect(&format!("host=localhost {} sslmode=disable", e.dsn)).expect("plaintext");
    assert_eq!(ssl_in_use(&mut conn), "false");
}
