//! pgmig's identifier quoting against the server's `quote_ident` for every keyword.
//!
//! pgmig uses the newest Postgres keyword list on every server, so it may quote a word an older
//! server leaves bare (`json` on 16). That is always valid SQL. Quoting less than the server is a
//! bug on any version; on the newest version the two must agree exactly.

use crate::support::{TestDb, postgres_dsn};

const NEWEST_MAJOR: u32 = 18;

#[test]
fn quote_ident_matches_server_for_keywords() {
    if postgres_dsn().is_none() {
        eprintln!("skipped on pgmem: no pg_get_keywords() (kaeawc/pgmem#164)");
        return;
    }
    let db = TestDb::new();
    let mut conn = db.connect();
    let version: u32 = conn.query("SHOW server_version_num").expect("server_version_num")[0][0]
        .as_deref()
        .and_then(|v| v.parse().ok())
        .expect("numeric server_version_num");
    let exact = version / 10000 >= NEWEST_MAJOR;

    let rows = conn.query("SELECT word, quote_ident(word) FROM pg_get_keywords() ORDER BY 1").expect("pg_get_keywords");
    assert!(!rows.is_empty());
    let mismatches: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            let word = r[0].clone().unwrap_or_default();
            let server = r[1].clone().unwrap_or_default();
            let ours = pgmig_model::quote_ident(&word);
            let under_quoted = server != word && ours == word;
            ((exact && ours != server) || under_quoted).then(|| format!("{word}: server {server}, pgmig {ours}"))
        })
        .collect();
    assert!(mismatches.is_empty(), "server_version_num {version}: {mismatches:#?}");
}
