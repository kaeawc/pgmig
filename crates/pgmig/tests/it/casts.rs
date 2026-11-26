//! `assignment_castable`: the server says which type changes need `USING`.

use crate::support::shadow;

#[test]
fn the_server_says_which_type_changes_need_using() {
    let (mut sh, _db) = shadow();
    sh.load("CREATE TYPE mood AS ENUM ('a'); CREATE DOMAIN d AS integer;").expect("load");
    let cases = [
        ("integer", "bigint", true),
        ("bigint", "integer", true),
        ("character varying(40)", "character varying(10)", true),
        ("public.mood", "text", true),
        ("integer[]", "bigint[]", true),
        ("public.d", "bigint", true),
        ("text", "integer", false),
        ("text", "public.mood", false),
        ("text[]", "integer[]", false),
        ("integer", "boolean", false),
        // A type the server does not have: answered with USING.
        ("public.gone", "integer", false),
    ];
    for (from, to, expected) in cases {
        assert_eq!(pgmig::assignment_castable(sh.conn(), from, to), expected, "{from} -> {to}");
    }
    let left = sh.conn().query("SELECT count(*) FROM pg_class WHERE relname = 'pgmig_cast_probe'").expect("query");
    assert_eq!(left, [[Some("0".to_owned())]], "the probe table is rolled back");
}
