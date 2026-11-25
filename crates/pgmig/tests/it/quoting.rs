//! pgmig's identifier quoting must agree with the server's `quote_ident` for every keyword.

use crate::support::{TestDb, postgres_dsn};

#[test]
fn quote_ident_matches_server_for_keywords() {
    if postgres_dsn().is_none() {
        eprintln!("skipped on pgmem: no pg_get_keywords() (kaeawc/pgmem#164)");
        return;
    }
    let db = TestDb::new();
    let mut conn = db.connect();
    let rows = conn.query("SELECT word, quote_ident(word) FROM pg_get_keywords() ORDER BY 1").expect("pg_get_keywords");
    assert!(!rows.is_empty());
    let mismatches: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            let word = r[0].clone().unwrap_or_default();
            let server = r[1].clone().unwrap_or_default();
            let ours = pgmig_model::quote_ident(&word);
            (ours != server).then(|| format!("{word}: server {server}, pgmig {ours}"))
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}
