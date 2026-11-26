//! `--include`, `--exclude` and `--schema` on the schema commands.

use crate::support::{SqlFile, pgmig, shadow_arg};

fn stdout(args: &[&str]) -> String {
    let out = pgmig(args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn inspect_and_diff_take_filters_and_a_schema() {
    let (shadow, _keep) = shadow_arg();
    let from = SqlFile::new("CREATE SCHEMA app; CREATE TABLE app.x (id int); CREATE TABLE tmp_a (id int);");
    let to = SqlFile::new("CREATE SCHEMA app; CREATE TABLE app.x (id int); CREATE TABLE tmp_b (id int); CREATE TABLE t (id int);");
    let inspect = stdout(&["schema", "inspect", &from.path(), "--shadow", &shadow, "--schema", "app"]);
    assert_eq!(inspect, "CREATE TABLE x (\n    id integer\n);\n");
    let args = ["schema", "diff", "--from", &from.path(), "--to", &to.path(), "--shadow", &shadow];
    let diff = stdout(&[&args[..], &["--exclude", "public.tmp_*", "--exclude", "schema:app"]].concat());
    assert_eq!(diff, "CREATE TABLE public.t (\n    id integer\n);\n");
    let diff = stdout(&[&args[..], &["--include", "table:public.tmp_?"]].concat());
    assert_eq!(diff, "CREATE TABLE public.tmp_b (\n    id integer\n);\nDROP TABLE public.tmp_a;\n");

    let bad = pgmig(&[&args[..], &["--include", "view:public.v"]].concat());
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("invalid filter pattern \"view:public.v\""));
}
