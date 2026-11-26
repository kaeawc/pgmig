//! The diff corpus: `corpus/<area>/<case>/{from.sql,to.sql,plan.sql}`.
//!
//! For each case: load `from` into a target and `to` into a shadow, diff, compare the plan with
//! `plan.sql` (`PGMIG_BLESS=1` rewrites it), apply the plan to the target (one transaction per
//! [`pgmig::Plan::batches`] entry), and check that nothing
//! is left to change. On pgmem, the failing cases must be exactly those in
//! `corpus/pgmem_known_failures.txt`; on Postgres, every case must pass.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use pgmig::{diff, diff_with_casts, inspect, plan};

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

fn run_case(dir: &Path) -> Result<(), String> {
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

    let planned = plan(&diff_with_casts(&a, &b, sh.conn())).map_err(|e| format!("planning: {e}"))?;
    let sql = planned.to_sql();
    let plan_file = dir.join("plan.sql");
    if std::env::var_os("PGMIG_BLESS").is_some() {
        fs::write(&plan_file, &sql).map_err(|e| format!("writing plan.sql: {e}"))?;
    } else {
        let expected = read_opt(&plan_file);
        if expected != sql {
            return Err(format!("plan differs from plan.sql (PGMIG_BLESS=1 to update)\n--- expected\n{expected}--- actual\n{sql}"));
        }
    }

    // One transaction per batch: a new enum value is only usable once committed.
    for batch in planned.batches() {
        conn.batch(&batch).map_err(|e| format!("applying the plan: {e}\n{sql}"))?;
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

    let mut failed = BTreeSet::new();
    let mut report = String::new();
    for (name, dir) in &cases {
        if let Err(e) = run_case(dir) {
            report.push_str(&format!("\n== {name} ({})\n{e}\n", backend_name()));
            failed.insert(name.clone());
        }
    }

    if postgres_dsn().is_some() {
        assert!(failed.is_empty(), "corpus cases fail on Postgres:{report}");
        return;
    }
    let known = known_failures();
    let new: Vec<_> = failed.difference(&known).collect();
    let fixed: Vec<_> = known.difference(&failed).collect();
    assert!(
        new.is_empty() && fixed.is_empty(),
        "corpus/pgmem_known_failures.txt is out of date.\nnewly failing: {new:?}\nnow passing (remove them): {fixed:?}{report}"
    );
}
