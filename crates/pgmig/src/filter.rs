//! Which objects pgmig manages: `--include` / `--exclude` patterns over `schema.object[.column]`.
//!
//! A pattern is `[kind:]glob`, where `kind` is `schema`, `table`, `enum` or `sequence` (no prefix
//! matches any kind) and `glob` has one part per level separated by `.`: `schema`, `schema.object`
//! or `schema.table.column`. Each part matches an unquoted name with `*` (any run of characters)
//! and `?` (one character); everything else matches itself. Names containing `.` cannot be named.
//!
//! An object is managed when it is included and not excluded:
//!
//! - with no includes, everything is included; otherwise an object is included when an include
//!   names it, names what contains it (a schema includes its objects, a table its columns), or
//!   names something inside it (`app.*` includes schema `app`, `public.t.a` includes table
//!   `public.t` with only column `a`);
//! - an object is excluded when an exclude names it or what contains it (`public` excludes the
//!   schema and everything in it; `public.tmp_*` excludes those objects but not the schema).
//!
//! Filtering ([`filter_model`]) removes unmanaged objects from a model; it is applied to both sides
//! of a diff, so unmanaged objects are never created, changed or dropped. A managed object may
//! still refer to an unmanaged one (a foreign key to an excluded table, a column of an excluded
//! enum type, a default using an excluded sequence): the reference is kept by name, and the plan
//! assumes the object exists. A constraint on an excluded column goes with the column, and so does
//! a sequence `OWNED BY` it (a `serial`'s): owned sequences are managed exactly when their column
//! is, whatever patterns name the sequence itself.
//!
//! A [`Filter`] serializes as `{"include": [...], "exclude": [...]}`, each pattern as the string it
//! was parsed from (in its canonical spelling, [`Pattern`]'s `Display`).

use std::fmt;

use pgmig_model::{Model, QualName};
use serde::{Deserialize, Serialize};

/// A pattern that is not `[kind:]glob` with one to three non-empty parts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid filter pattern {pattern:?}: {reason}")]
pub struct PatternError {
    pub pattern: String,
    pub reason: &'static str,
}

/// The kind a pattern names with its prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Schema,
    Table,
    Enum,
    Sequence,
}

/// One `--include` or `--exclude` pattern. Serialized as its string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Pattern {
    pub kind: Option<Kind>,
    /// One to three globs: schema, object, column.
    pub parts: Vec<String>,
}

impl Pattern {
    pub fn parse(s: &str) -> Result<Pattern, PatternError> {
        let err = |reason| PatternError { pattern: s.to_owned(), reason };
        let (kind, glob) = match s.split_once(':') {
            Some((k, rest)) => {
                let kind = match k {
                    "schema" => Kind::Schema,
                    "table" => Kind::Table,
                    "enum" => Kind::Enum,
                    "sequence" => Kind::Sequence,
                    _ => return Err(err("the kind must be schema, table, enum or sequence")),
                };
                (Some(kind), rest)
            }
            None => (None, s),
        };
        let parts: Vec<String> = glob.split('.').map(str::to_owned).collect();
        if parts.iter().any(String::is_empty) || parts.len() > 3 {
            return Err(err("expected schema, schema.object or schema.table.column"));
        }
        match (kind, parts.len()) {
            (Some(Kind::Schema), n) if n != 1 => Err(err("a schema pattern has one part")),
            (Some(Kind::Table | Kind::Enum | Kind::Sequence), 1) => Err(err("an object pattern needs schema.object")),
            (Some(Kind::Enum | Kind::Sequence), 3) => Err(err("only tables have columns")),
            _ => Ok(Pattern { kind, parts }),
        }
    }

    /// How this pattern relates to `object`: `Some(n)` when the parts both have match, where `n`
    /// compares the pattern's depth to the object's.
    fn relate(&self, object: &Path<'_>) -> Option<std::cmp::Ordering> {
        let kind_ok = match self.kind {
            None | Some(Kind::Schema) => true,
            // A schema contains objects of every kind.
            Some(k) => object.names.len() == 1 || object.kind == Some(k),
        };
        let common = self.parts.len().min(object.names.len());
        let parts_ok = self.parts[..common].iter().zip(&object.names[..common]).all(|(g, n)| glob(g, n));
        (kind_ok && parts_ok).then(|| self.parts.len().cmp(&object.names.len()))
    }
}

impl fmt::Display for Pattern {
    /// The pattern as [`Pattern::parse`] reads it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            None => "",
            Some(Kind::Schema) => "schema:",
            Some(Kind::Table) => "table:",
            Some(Kind::Enum) => "enum:",
            Some(Kind::Sequence) => "sequence:",
        };
        write!(f, "{kind}{}", self.parts.join("."))
    }
}

impl TryFrom<String> for Pattern {
    type Error = PatternError;
    fn try_from(s: String) -> Result<Pattern, PatternError> {
        Pattern::parse(&s)
    }
}

impl From<Pattern> for String {
    fn from(p: Pattern) -> String {
        p.to_string()
    }
}

/// An object as patterns see it: its names from the schema down, and the kind of its second level
/// (a column's is its table's).
struct Path<'a> {
    kind: Option<Kind>,
    names: Vec<&'a str>,
}

/// `--include` and `--exclude` patterns; the default manages everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub include: Vec<Pattern>,
    pub exclude: Vec<Pattern>,
}

impl Filter {
    /// A filter from pattern strings.
    pub fn parse<I: AsRef<str>, E: AsRef<str>>(include: &[I], exclude: &[E]) -> Result<Filter, PatternError> {
        Ok(Filter {
            include: include.iter().map(|p| Pattern::parse(p.as_ref())).collect::<Result<_, _>>()?,
            exclude: exclude.iter().map(|p| Pattern::parse(p.as_ref())).collect::<Result<_, _>>()?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    fn manages(&self, object: &Path<'_>) -> bool {
        let included = self.include.is_empty() || self.include.iter().any(|p| p.relate(object).is_some());
        let excluded = self.exclude.iter().any(|p| p.relate(object).is_some_and(|o| o.is_le()));
        included && !excluded
    }

    pub fn manages_schema(&self, schema: &str) -> bool {
        self.manages(&Path { kind: Some(Kind::Schema), names: vec![schema] })
    }

    pub fn manages_object(&self, kind: Kind, name: &QualName) -> bool {
        self.manages(&Path { kind: Some(kind), names: vec![&name.schema, &name.name] })
    }

    pub fn manages_column(&self, table: &QualName, column: &str) -> bool {
        self.manages(&Path { kind: Some(Kind::Table), names: vec![&table.schema, &table.name, column] })
    }
}

/// `model` without the objects `filter` does not manage (see the module doc).
pub fn filter_model(model: &Model, filter: &Filter) -> Model {
    if filter.is_empty() {
        return model.clone();
    }
    let mut m = model.clone();
    m.schemas.retain(|name, _| filter.manages_schema(name));
    m.enums.retain(|name, _| filter.manages_object(Kind::Enum, name));
    // A sequence `OWNED BY` a column follows that column: it is created and dropped with it.
    m.sequences.retain(|name, seq| match &seq.owned_by {
        Some(o) => filter.manages_object(Kind::Table, &o.table) && filter.manages_column(&o.table, &o.column),
        None => filter.manages_object(Kind::Sequence, name),
    });
    m.tables.retain(|name, _| filter.manages_object(Kind::Table, name));
    for t in m.tables.values_mut() {
        let name = t.name.clone();
        let gone: Vec<String> = t.columns.iter().filter(|c| !filter.manages_column(&name, &c.name)).map(|c| c.name.clone()).collect();
        if gone.is_empty() {
            continue;
        }
        t.columns.retain(|c| !gone.contains(&c.name));
        t.constraints.retain(|c| !c.columns.iter().any(|col| gone.contains(col)));
    }
    m
}

/// `model` reduced to the objects in `schema`, without the schema itself: what pgmig manages when
/// scoped to one schema.
pub fn scope_model(model: &Model, schema: &str) -> Model {
    let mut m = model.clone();
    m.schemas.clear();
    m.tables.retain(|n, _| n.schema == schema);
    m.enums.retain(|n, _| n.schema == schema);
    m.sequences.retain(|n, _| n.schema == schema);
    m
}

/// Whether `name` matches `pattern`, where `*` matches any run of characters and `?` one.
pub fn glob(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // Backtrack to the last `*`, letting it take one more character.
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgmig_model::{Column, Constraint, ConstraintKind, Enum, Schema, Table};

    #[test]
    fn filters_serialize_as_pattern_strings() {
        let f = Filter::parse(&["app", "table:public.t*.a?", "enum:x.e", "sequence:x.s", "schema:s"], &["public.tmp_*"]).expect("patterns");
        let json = serde_json::to_string(&f).expect("serialize");
        assert_eq!(json, r#"{"include":["app","table:public.t*.a?","enum:x.e","sequence:x.s","schema:s"],"exclude":["public.tmp_*"]}"#);
        assert_eq!(serde_json::from_str::<Filter>(&json).expect("deserialize"), f);
        assert_eq!(serde_json::from_str::<Filter>("{}").expect("defaults"), Filter::default());
        let err = serde_json::from_str::<Filter>(r#"{"include":["view:public.v"]}"#).expect_err("bad pattern");
        assert!(err.to_string().contains("invalid filter pattern \"view:public.v\""), "{err}");
    }

    #[test]
    fn globs_match_star_and_question_mark() {
        assert!(glob("tmp_*", "tmp_"));
        assert!(glob("tmp_*", "tmp_abc"));
        assert!(!glob("tmp_*", "tmp"));
        assert!(!glob("tmp_*", "xtmp_a"));
        assert!(glob("*", ""));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "ac"));
        assert!(glob("*_log_*", "a_log_b_log_c"));
        assert!(glob("a*b*c", "aXbYbZc"));
        assert!(!glob("a*b*c", "aXbYbZ"));
        assert!(glob("Users", "Users"));
        assert!(!glob("users", "Users"));
        assert!(glob("é?", "éa"));
    }

    #[test]
    fn patterns_parse_kind_and_parts() {
        let p = Pattern::parse("table:public.tmp_*").expect("parse");
        assert_eq!(p, Pattern { kind: Some(Kind::Table), parts: vec!["public".into(), "tmp_*".into()] });
        assert_eq!(Pattern::parse("public.t.c").expect("parse").parts.len(), 3);
        assert_eq!(Pattern::parse("schema:app").expect("parse").kind, Some(Kind::Schema));
        for bad in ["", "a..b", "a.b.c.d", "view:a.b", "schema:a.b", "table:a", "enum:a.b.c", "public."] {
            assert!(Pattern::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            Pattern::parse("view:a.b").expect_err("bad").to_string(),
            "invalid filter pattern \"view:a.b\": the kind must be schema, table, enum or sequence"
        );
    }

    fn q(s: &str, n: &str) -> QualName {
        QualName::new(s, n)
    }

    #[test]
    fn excludes_name_objects_and_their_contents() {
        let f = Filter::parse::<&str, _>(&[], &["public.tmp_*"]).expect("filter");
        assert!(f.manages_schema("public"));
        assert!(!f.manages_object(Kind::Table, &q("public", "tmp_a")));
        assert!(!f.manages_object(Kind::Enum, &q("public", "tmp_a")));
        assert!(!f.manages_column(&q("public", "tmp_a"), "id"));
        assert!(f.manages_object(Kind::Table, &q("public", "users")));
        assert!(f.manages_object(Kind::Table, &q("app", "tmp_a")));

        let f = Filter::parse::<&str, _>(&[], &["app"]).expect("filter");
        assert!(!f.manages_schema("app"));
        assert!(!f.manages_object(Kind::Sequence, &q("app", "s")));
        assert!(f.manages_schema("public"));
    }

    #[test]
    fn kind_prefixes_limit_patterns_to_one_kind() {
        let f = Filter::parse::<&str, _>(&[], &["enum:public.*"]).expect("filter");
        assert!(!f.manages_object(Kind::Enum, &q("public", "mood")));
        assert!(f.manages_object(Kind::Table, &q("public", "mood")));
        assert!(f.manages_schema("public"));

        let f = Filter::parse(&["table:public.*"], &[] as &[&str]).expect("filter");
        assert!(f.manages_schema("public"), "a schema holding included objects is included");
        assert!(f.manages_object(Kind::Table, &q("public", "t")));
        assert!(f.manages_column(&q("public", "t"), "c"));
        assert!(!f.manages_object(Kind::Sequence, &q("public", "t_id_seq")));
        assert!(!f.manages_schema("app"));
    }

    #[test]
    fn column_patterns_include_their_table_and_exclude_only_the_column() {
        let f = Filter::parse(&["public.t.a"], &[] as &[&str]).expect("filter");
        assert!(f.manages_object(Kind::Table, &q("public", "t")));
        assert!(f.manages_column(&q("public", "t"), "a"));
        assert!(!f.manages_column(&q("public", "t"), "b"));
        assert!(!f.manages_object(Kind::Table, &q("public", "u")));

        let f = Filter::parse::<&str, _>(&[], &["public.*.secret_*"]).expect("filter");
        assert!(f.manages_object(Kind::Table, &q("public", "t")));
        assert!(!f.manages_column(&q("public", "t"), "secret_key"));
        assert!(f.manages_column(&q("public", "t"), "id"));
    }

    fn column(name: &str) -> Column {
        Column {
            name: name.into(),
            type_name: "integer".into(),
            type_ref: None,
            not_null: false,
            default: None,
            comment: None,
            default_sequences: vec![],
            identity: None,
        }
    }

    #[test]
    fn filtering_a_model_drops_unmanaged_objects_and_constraints_on_unmanaged_columns() {
        let mut m = Model::empty_database();
        m.schemas.insert("app".into(), Schema { name: "app".into(), comment: None });
        let check = Constraint {
            name: "t_legacy_check".into(),
            kind: ConstraintKind::Check,
            definition: "CHECK ((legacy > 0))".into(),
            deferrable: false,
            deferred: false,
            validated: true,
            columns: vec!["legacy".into()],
            references: None,
            comment: None,
        };
        let t = Table { name: q("public", "t"), columns: vec![column("id"), column("legacy")], comment: None, constraints: vec![check] };
        m.tables.insert(t.name.clone(), t);
        let tmp = Table { name: q("public", "tmp_x"), columns: vec![], comment: None, constraints: vec![] };
        m.tables.insert(tmp.name.clone(), tmp);
        m.enums.insert(q("app", "mood"), Enum { name: q("app", "mood"), labels: vec![], comment: None });

        let f = Filter::parse::<&str, _>(&[], &["public.tmp_*", "public.t.legacy", "schema:app"]).expect("filter");
        let out = filter_model(&m, &f);
        assert_eq!(out.schemas.keys().collect::<Vec<_>>(), ["public"]);
        assert_eq!(out.tables.keys().collect::<Vec<_>>(), [&q("public", "t")]);
        let t = &out.tables[&q("public", "t")];
        assert_eq!(t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id"]);
        assert!(t.constraints.is_empty());
        assert!(out.enums.is_empty());
        assert_eq!(filter_model(&m, &Filter::default()), m);

        let scoped = scope_model(&m, "public");
        assert!(scoped.schemas.is_empty() && scoped.enums.is_empty());
        assert_eq!(scoped.tables.len(), 2);
    }
}
