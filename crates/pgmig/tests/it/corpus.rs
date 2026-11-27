//! The diff corpus: `corpus/<area>/<case>/{from.sql,to.sql,plan.sql}`, and optionally `options.toml`.
//!
//! For each case: load `from` into a target and `to` into a shadow, diff, compare the plan with
//! `plan.sql` (`PGMIG_BLESS=1` rewrites it), apply the plan to the target (one transaction per
//! transactional [`pgmig::Plan::units`] entry, each non-transactional statement on its own outside
//! one), and check that nothing is left to change.
//!
//! `options.toml` sets the case's plan options; a missing file is the defaults, and an unknown key
//! or a value of the wrong type fails the case. The keys are those of [`pgmig::PlanOptions`]:
//! `concurrent_indexes` (boolean, default false). On pgmem, the failing cases must be exactly those in
//! `corpus/pgmem_known_failures.txt`; on Postgres, every case must pass.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use pgmig::{PlanOptions, diff, diff_with_casts, inspect, plan_with};

use crate::support::{TestDb, backend_name, postgres_dsn, shadow};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}

fn cases() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for area in dirs(&root()) {
        for case in dirs(&area) {
            let name = format!("{}/{}", file_name(&area), file_name(&case));
            out.push((name, case));
        }
    }
    out
}

fn dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> =
        fs::read_dir(p).expect("read corpus dir").filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
    v.sort();
    v
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn read_opt(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default()
}

/// The plan options of the case in `dir`: `options.toml`, or the defaults.
fn case_options(dir: &Path) -> Result<PlanOptions, String> {
    let text = read_opt(&dir.join("options.toml"));
    let table: toml::Table = text.parse().map_err(|e| format!("options.toml: {e}"))?;
    let mut opts = PlanOptions::default();
    for (key, value) in &table {
        match (key.as_str(), value) {
            ("concurrent_indexes", toml::Value::Boolean(b)) => opts.concurrent_indexes = *b,
            ("concurrent_indexes", _) => return Err("options.toml: concurrent_indexes: expected a boolean".into()),
            _ => return Err(format!("options.toml: unknown key `{key}` (expected `concurrent_indexes`)")),
        }
    }
    Ok(opts)
}

fn run_case(name: &str, dir: &Path, known: &BTreeSet<String>) -> Result<(), String> {
    let target = TestDb::new();
    let mut conn = target.connect();
    let from = read_opt(&dir.join("from.sql"));
    if !from.trim().is_empty() {
        conn.batch(&from).map_err(|e| format!("loading from.sql into the target: {e}"))?;
    }
    let a = inspect(&mut conn).map_err(|e| format!("inspecting the target: {e}"))?;

    let (mut sh, _shadow_db) = shadow();
    sh.load(&read_opt(&dir.join("to.sql"))).map_err(|e| format!("loading to.sql into the shadow: {e}"))?;
    let b = inspect(sh.conn()).map_err(|e| format!("inspecting the shadow: {e}"))?;

    let planned = plan_with(&diff_with_casts(&a, &b, sh.conn()), &case_options(dir)?).map_err(|e| format!("planning: {e}"))?;
    let sql = planned.to_sql();
    let plan_file = dir.join("plan.sql");
    let write = bless() && should_write_plan(postgres_dsn().is_some(), name, known);
    if bless() && !write {
        println!("bless: skipped {name} (known pgmem failure; its plan.sql is hand-written from Postgres)");
    }
    if write {
        fs::write(&plan_file, &sql).map_err(|e| format!("writing plan.sql: {e}"))?;
    } else {
        let expected = read_opt(&plan_file);
        if expected != sql {
            return Err(format!("plan differs from plan.sql (PGMIG_BLESS=1 to update)\n--- expected\n{expected}--- actual\n{sql}"));
        }
    }

    // One transaction per unit, a statement outside a transaction on its own: a new enum value is
    // only usable once committed, and `CREATE INDEX CONCURRENTLY` cannot run in a transaction.
    for unit in planned.units() {
        let script = planned.unit_sql(&unit);
        let script = if unit.transactional { format!("BEGIN;\n{script}COMMIT;") } else { script };
        conn.batch(&script).map_err(|e| format!("applying the plan: {e}\n{sql}"))?;
    }
    let applied = inspect(&mut conn).map_err(|e| format!("inspecting after apply: {e}"))?;
    let left = diff(&applied, &b);
    if !left.is_empty() {
        return Err(format!("changes left after applying the plan: {left:?}"));
    }
    if !diff(&b, &b).is_empty() {
        return Err("diff of the desired state with itself is not empty".into());
    }
    Ok(())
}

fn bless() -> bool {
    std::env::var_os("PGMIG_BLESS").is_some()
}

/// Blessing on pgmem must not overwrite the hand-written plans of known pgmem failures; on real
/// Postgres every case is rewritten.
fn should_write_plan(on_postgres: bool, case: &str, known: &BTreeSet<String>) -> bool {
    on_postgres || !known.contains(case)
}

fn known_failures() -> BTreeSet<String> {
    read_opt(&root().join("pgmem_known_failures.txt"))
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn corpus() {
    let cases = cases();
    assert!(!cases.is_empty(), "no corpus cases under {}", root().display());

    let known = known_failures();
    let mut failed = BTreeSet::new();
    let mut report = String::new();
    for (name, dir) in &cases {
        if let Err(e) = run_case(name, dir, &known) {
            report.push_str(&format!("\n== {name} ({})\n{e}\n", backend_name()));
            failed.insert(name.clone());
        }
    }

    if postgres_dsn().is_some() {
        assert!(failed.is_empty(), "corpus cases fail on Postgres:{report}");
        return;
    }
    let new: Vec<_> = failed.difference(&known).collect();
    let fixed: Vec<_> = known.difference(&failed).collect();
    assert!(
        new.is_empty() && fixed.is_empty(),
        "corpus/pgmem_known_failures.txt is out of date.\nnewly failing: {new:?}\nnow passing (remove them): {fixed:?}{report}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_options_are_read_from_options_toml() {
        let dir = std::env::temp_dir().join(format!("pgmig-corpus-options-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        assert_eq!(case_options(&dir), Ok(PlanOptions::default()));
        for (text, want) in [
            ("concurrent_indexes = true\n", Ok(true)),
            ("# plain\nconcurrent_indexes = false\n", Ok(false)),
            ("concurrent_indexes = 1\n", Err("options.toml: concurrent_indexes: expected a boolean")),
            ("concurent_indexes = true\n", Err("options.toml: unknown key `concurent_indexes` (expected `concurrent_indexes`)")),
        ] {
            fs::write(dir.join("options.toml"), text).expect("write");
            let got = case_options(&dir).map(|o| o.concurrent_indexes);
            assert_eq!(got, want.map_err(str::to_owned), "{text}");
        }
        fs::write(dir.join("options.toml"), "concurrent_indexes = \n").expect("write");
        assert!(case_options(&dir).expect_err("syntax").starts_with("options.toml: "));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bless_skips_known_failures_only_on_pgmem() {
        let known: BTreeSet<String> = ["a/b".to_owned()].into();
        assert!(!should_write_plan(false, "a/b", &known));
        assert!(should_write_plan(false, "a/c", &known));
        assert!(should_write_plan(true, "a/b", &known));
    }
}
