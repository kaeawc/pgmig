//! The schema model: what pgmig knows about a database, as read from its catalog.
//!
//! Object bodies hold exactly what the server prints (`format_type`, `pg_get_expr`, ...), so two
//! objects are equal when Postgres would print them identically.
//!
//! # Serialization
//!
//! Every type is serde-serializable, with a representation that is part of pgmig's interface (the
//! JSON output and the language packages read it): fields keep their Rust names, enums without data
//! are snake_case strings (`"primary_key"`), a [`QualName`] is `{"schema": ..., "name": ...}`, and
//! the maps of a [`Model`] are lists of their objects in key order (each object carries its own
//! name, and a `QualName` cannot be a JSON object key). Text the server printed stays a string.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

mod fingerprint;
pub use fingerprint::{FORMAT as FINGERPRINT_FORMAT, canonical, fingerprint};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    #[serde(with = "as_list")]
    pub schemas: BTreeMap<String, Schema>,
    #[serde(with = "as_list")]
    pub tables: BTreeMap<QualName, Table>,
    #[serde(with = "as_list")]
    pub enums: BTreeMap<QualName, Enum>,
    /// Standalone sequences, including those `OWNED BY` a column (a `serial` column's sequence).
    /// Identity columns' sequences are not here: they belong to the column ([`Column::identity`]).
    #[serde(with = "as_list")]
    pub sequences: BTreeMap<QualName, Sequence>,
}

impl Model {
    /// The model of a freshly created database: only the `public` schema, with the comment
    /// Postgres gives it ([`PUBLIC_SCHEMA_COMMENT`]).
    pub fn empty_database() -> Model {
        let mut m = Model::default();
        m.schemas.insert("public".to_owned(), Schema { name: "public".to_owned(), comment: Some(PUBLIC_SCHEMA_COMMENT.to_owned()) });
        m
    }
}

/// The comment a new database's `public` schema carries (Postgres 15 and later).
pub const PUBLIC_SCHEMA_COMMENT: &str = "standard public schema";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QualName {
    pub schema: String,
    pub name: String,
}

impl QualName {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> QualName {
        QualName { schema: schema.into(), name: name.into() }
    }
}

impl fmt::Display for QualName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", quote_ident(&self.schema), quote_ident(&self.name))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    /// `obj_description` of the schema.
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    pub name: QualName,
    /// In `attnum` order. Postgres can't reorder columns, so diffs match columns by name.
    pub columns: Vec<Column>,
    /// `obj_description` of the table.
    pub comment: Option<String>,
    /// Primary key, unique, check, foreign key and exclusion constraints defined on the table
    /// itself (not inherited), by name. `NOT NULL` is [`Column::not_null`], not a constraint here.
    pub constraints: Vec<Constraint>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn constraint(&self, name: &str) -> Option<&Constraint> {
        self.constraints.iter().find(|c| c.name == name)
    }
}

/// `pg_constraint.contype`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    PrimaryKey,
    Unique,
    Check,
    ForeignKey,
    Exclusion,
}

/// A table constraint. Its body is [`Constraint::definition`], as the server prints it; the other
/// fields are read from the catalog alongside it so the planner can order steps, and say nothing
/// the definition doesn't.
///
/// A constraint always lives in its table's schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub name: String,
    pub kind: ConstraintKind,
    /// `pg_get_constraintdef`, including `DEFERRABLE`, `INITIALLY DEFERRED` and `NOT VALID`.
    pub definition: String,
    /// `condeferrable`.
    pub deferrable: bool,
    /// `condeferred` (`INITIALLY DEFERRED`).
    pub deferred: bool,
    /// `convalidated`: false for `NOT VALID` (and `NOT ENFORCED`) constraints.
    pub validated: bool,
    /// The constrained columns (`conkey`); for a check, the columns its expression uses.
    pub columns: Vec<String>,
    /// What a foreign key references.
    pub references: Option<References>,
    /// `obj_description` of the constraint.
    pub comment: Option<String>,
}

/// The referenced side of a foreign key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct References {
    pub table: QualName,
    /// `confkey`.
    pub columns: Vec<String>,
    /// The primary key, unique or exclusion constraint whose index the foreign key uses: the
    /// index's name (`conindid`), which is the constraint's when a constraint owns it. `None` when
    /// the catalog doesn't say.
    pub constraint: Option<String>,
}

impl Constraint {
    /// [`Constraint::definition`] without `DEFERRABLE` and `INITIALLY DEFERRED`, which the server
    /// prints after the body and before `NOT ENFORCED` / `NOT VALID`. Two constraints whose
    /// definitions differ only there differ only in deferrability.
    pub fn definition_without_deferrability(&self) -> String {
        let (mut body, tail) = [" NOT ENFORCED", " NOT VALID"]
            .iter()
            .find_map(|t| self.definition.strip_suffix(t).map(|b| (b, *t)))
            .unwrap_or((self.definition.as_str(), ""));
        if self.deferred {
            body = body.strip_suffix(" INITIALLY DEFERRED").unwrap_or(body);
        }
        if self.deferrable {
            body = body.strip_suffix(" DEFERRABLE").unwrap_or(body);
        }
        format!("{body}{tail}")
    }
}

/// An enum type (`pg_type.typtype = 'e'`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enum {
    pub name: QualName,
    /// `pg_enum.enumlabel` in `enumsortorder` order.
    pub labels: Vec<String>,
    /// `obj_description` of the type.
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    /// `format_type(atttypid, atttypmod)`.
    pub type_name: String,
    /// The user-defined type the column uses (the element type for an array), from `pg_type`;
    /// `None` for types in system schemas. A table's row type has the table's name.
    pub type_ref: Option<QualName>,
    pub not_null: bool,
    /// `pg_get_expr(adbin, adrelid)`.
    pub default: Option<String>,
    /// `col_description` of the column.
    pub comment: Option<String>,
    /// The sequences the default takes values from (`nextval('<sequence>'::regclass)` in
    /// [`Column::default`], resolved by the server), so the planner can order the default
    /// against them. Empty without a default.
    pub default_sequences: Vec<QualName>,
    /// `GENERATED ... AS IDENTITY` (`attidentity`), with its sequence's options.
    pub identity: Option<Identity>,
}

/// How an identity column takes values (`pg_attribute.attidentity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Generated {
    /// `GENERATED ALWAYS` (`a`).
    Always,
    /// `GENERATED BY DEFAULT` (`d`).
    ByDefault,
}

/// An identity column: how it is generated and the options of its implicit sequence, whose type
/// is always the column's type.
///
/// Equality ignores [`Identity::sequence`]: the name is whatever Postgres picked when the
/// identity was added (truncated, or with a digit appended when the name was taken), not part of
/// the desired state.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub generated: Generated,
    pub options: SequenceOptions,
    /// The implicit sequence's name, from the catalog (`pg_depend`, `deptype = 'i'`); `None` when
    /// unknown (see [`Identity::sequence_name`]). Only used to order plan steps.
    pub sequence: Option<QualName>,
}

impl PartialEq for Identity {
    fn eq(&self, other: &Identity) -> bool {
        self.generated == other.generated && self.options == other.options
    }
}

impl Identity {
    /// The implicit sequence's name: [`Identity::sequence`], or else the name Postgres gives it
    /// when that name is free and short enough (`<table>_<column>_seq`).
    pub fn sequence_name(&self, table: &QualName, column: &str) -> QualName {
        self.sequence.clone().unwrap_or_else(|| QualName::new(table.schema.clone(), format!("{}_{column}_seq", table.name)))
    }
}

/// A standalone sequence (`pg_class.relkind = 'S'` that is not an identity column's).
///
/// A `serial` column is an integer column whose default is `nextval('<table>_<column>_seq')`
/// and whose sequence is `OWNED BY` it: the model keeps both halves as Postgres stores them (the
/// column's [`Column::default`] and this sequence with [`Sequence::owned_by`]), so a `serial`
/// column compares equal to the same column read back from any database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sequence {
    pub name: QualName,
    /// `format_type(seqtypid, NULL)`: `smallint`, `integer` or `bigint`.
    pub data_type: String,
    pub options: SequenceOptions,
    /// `OWNED BY`: the column whose drop drops the sequence (`pg_depend`, `deptype = 'a'`). The
    /// table is always in the sequence's schema.
    pub owned_by: Option<OwnedBy>,
    /// `obj_description` of the sequence.
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedBy {
    pub table: QualName,
    pub column: String,
}

/// A sequence's options as `pg_sequence` reports them (`seqstart`, `seqincrement`, `seqmin`,
/// `seqmax`, `seqcache`, `seqcycle`). The current value is state, not schema, and is not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceOptions {
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
}

impl SequenceOptions {
    /// The options Postgres gives a sequence of `data_type` created with `increment` and nothing
    /// else, or `None` for a type that is not `smallint`, `integer` or `bigint`.
    pub fn defaults(data_type: &str, increment: i64) -> Option<SequenceOptions> {
        let (type_min, type_max) = integer_bounds(data_type)?;
        let (min, max) = if increment > 0 { (1, type_max) } else { (type_min, -1) };
        let start = if increment > 0 { min } else { max };
        Some(SequenceOptions { start, increment, min, max, cache: 1, cycle: false })
    }
}

/// The range of a sequence data type as `format_type` prints it.
pub fn integer_bounds(data_type: &str) -> Option<(i64, i64)> {
    match data_type {
        "smallint" => Some((i16::MIN.into(), i16::MAX.into())),
        "integer" => Some((i32::MIN.into(), i32::MAX.into())),
        "bigint" => Some((i64::MIN, i64::MAX)),
        _ => None,
    }
}

/// Quote a string as a SQL literal that reads back the same whatever the target's
/// `standard_conforming_strings` is: a plain `'...'` (quotes doubled) when the text has no
/// backslash, otherwise an escape string `E'...'` with backslashes and quotes doubled.
pub fn quote_literal(text: &str) -> String {
    let escape = text.contains('\\');
    let mut out = String::with_capacity(text.len() + 3);
    if escape {
        out.push('E');
    }
    out.push('\'');
    for c in text.chars() {
        if c == '\'' || (escape && c == '\\') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Quote an identifier the way Postgres' `quote_ident` does.
pub fn quote_ident(ident: &str) -> String {
    let plain = ident.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && ident.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
        && !is_keyword(ident);
    if plain { ident.to_owned() } else { format!("\"{}\"", ident.replace('"', "\"\"")) }
}

/// Keywords `quote_ident` quotes: every Postgres keyword except the unreserved ones.
fn is_keyword(word: &str) -> bool {
    QUOTED_KEYWORDS.binary_search(&word).is_ok()
}

// Reserved, type-function-name and column-name keywords (PostgreSQL 18, src/include/parser/kwlist.h).
const QUOTED_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// An object a [`Model`] map holds, keyed by its own name.
trait Keyed {
    type Key: Ord;
    fn key(&self) -> Self::Key;
}

impl Keyed for Schema {
    type Key = String;
    fn key(&self) -> String {
        self.name.clone()
    }
}

macro_rules! keyed_by_qualname {
    ($($t:ty),*) => {$(
        impl Keyed for $t {
            type Key = QualName;
            fn key(&self) -> QualName {
                self.name.clone()
            }
        }
    )*};
}
keyed_by_qualname!(Table, Enum, Sequence);

/// A `BTreeMap` of [`Keyed`] objects as a list of them in key order; a list naming one object twice
/// is refused.
mod as_list {
    use std::collections::BTreeMap;

    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Keyed;

    pub fn serialize<V: Keyed + Serialize, S: Serializer>(map: &BTreeMap<V::Key, V>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(map.values())
    }

    pub fn deserialize<'de, V, D>(d: D) -> Result<BTreeMap<V::Key, V>, D::Error>
    where
        V: Keyed + Deserialize<'de>,
        D: Deserializer<'de>,
    {
        let mut map = BTreeMap::new();
        for v in Vec::<V>::deserialize(d)? {
            if map.insert(v.key(), v).is_some() {
                return Err(D::Error::custom("an object is listed twice"));
            }
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_sorted() {
        assert!(QUOTED_KEYWORDS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn literal_quoting() {
        assert_eq!(quote_literal("plain"), "'plain'");
        assert_eq!(quote_literal(""), "''");
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal("''"), "''''''");
        assert_eq!(quote_literal("a\\b"), "E'a\\\\b'");
        assert_eq!(quote_literal("a\\'b"), "E'a\\\\''b'");
        assert_eq!(quote_literal("\\"), "E'\\\\'");
        assert_eq!(quote_literal("tab\\\nnew"), "E'tab\\\\\nnew'");
        assert_eq!(quote_literal("caf\u{e9} \u{1F600}"), "'caf\u{e9} \u{1F600}'");
        assert_eq!(quote_literal("caf\u{e9}\\"), "E'caf\u{e9}\\\\'");
        assert_eq!(quote_literal("line1\nline2"), "'line1\nline2'");
    }

    #[test]
    fn sequence_defaults_follow_the_type_and_direction() {
        let up = SequenceOptions::defaults("integer", 1).expect("integer");
        assert_eq!(up, SequenceOptions { start: 1, increment: 1, min: 1, max: 2147483647, cache: 1, cycle: false });
        let down = SequenceOptions::defaults("smallint", -2).expect("smallint");
        assert_eq!(down, SequenceOptions { start: -1, increment: -2, min: -32768, max: -1, cache: 1, cycle: false });
        assert_eq!(SequenceOptions::defaults("bigint", 1).map(|o| o.max), Some(i64::MAX));
        assert_eq!(SequenceOptions::defaults("numeric", 1), None);
    }

    #[test]
    fn deferrability_is_stripped_from_the_printed_definition() {
        let fk = |definition: &str, deferrable, deferred| Constraint {
            name: "fk".into(),
            kind: ConstraintKind::ForeignKey,
            definition: definition.into(),
            deferrable,
            deferred,
            validated: true,
            columns: vec![],
            references: None,
            comment: None,
        };
        let base = "FOREIGN KEY (a) REFERENCES p(id) ON DELETE CASCADE";
        assert_eq!(fk(base, false, false).definition_without_deferrability(), base);
        assert_eq!(fk(&format!("{base} DEFERRABLE"), true, false).definition_without_deferrability(), base);
        assert_eq!(fk(&format!("{base} DEFERRABLE INITIALLY DEFERRED"), true, true).definition_without_deferrability(), base);
        assert_eq!(
            fk(&format!("{base} DEFERRABLE INITIALLY DEFERRED NOT VALID"), true, true).definition_without_deferrability(),
            format!("{base} NOT VALID")
        );
        // Only what the flags say the server printed is removed.
        assert_eq!(fk(&format!("{base} DEFERRABLE"), false, false).definition_without_deferrability(), format!("{base} DEFERRABLE"));
    }

    #[test]
    fn empty_database_has_the_public_comment() {
        let m = Model::empty_database();
        assert_eq!(m.schemas["public"].comment.as_deref(), Some("standard public schema"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("users"), "users");
        assert_eq!(quote_ident("_a1$"), "_a1$");
        assert_eq!(quote_ident("Users"), "\"Users\"");
        assert_eq!(quote_ident("user"), "\"user\"");
        assert_eq!(quote_ident("1a"), "\"1a\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("name"), "name");
    }

    /// A model with one of everything: schemas, tables with columns of every shape, constraints,
    /// enums, sequences and comments.
    fn sample() -> Model {
        let t = QualName::new("app", "users");
        let ints = SequenceOptions::defaults("integer", 1).expect("integer");
        let mut m = Model::empty_database();
        m.schemas.insert("app".into(), Schema { name: "app".into(), comment: None });
        let pk = Constraint {
            name: "users_pkey".into(),
            kind: ConstraintKind::PrimaryKey,
            definition: "PRIMARY KEY (id)".into(),
            deferrable: false,
            deferred: false,
            validated: true,
            columns: vec!["id".into()],
            references: None,
            comment: Some("the key".into()),
        };
        let fk = Constraint {
            name: "users_team_fkey".into(),
            kind: ConstraintKind::ForeignKey,
            definition: "FOREIGN KEY (team) REFERENCES app.teams(id) DEFERRABLE".into(),
            deferrable: true,
            deferred: false,
            validated: false,
            columns: vec!["team".into()],
            references: Some(References {
                table: QualName::new("app", "teams"),
                columns: vec!["id".into()],
                constraint: Some("teams_pkey".into()),
            }),
            comment: None,
        };
        let column = |name: &str, type_name: &str| Column {
            name: name.into(),
            type_name: type_name.into(),
            type_ref: None,
            not_null: false,
            default: None,
            comment: None,
            default_sequences: vec![],
            identity: None,
        };
        let columns = vec![
            Column {
                identity: Some(Identity {
                    generated: Generated::ByDefault,
                    options: ints,
                    sequence: Some(QualName::new("app", "users_id_seq")),
                }),
                not_null: true,
                ..column("id", "integer")
            },
            Column {
                default: Some("nextval('app.users_n_seq'::regclass)".into()),
                default_sequences: vec![QualName::new("app", "users_n_seq")],
                not_null: true,
                ..column("n", "integer")
            },
            Column { type_ref: Some(QualName::new("app", "mood")), comment: Some("how they feel".into()), ..column("mood", "app.mood[]") },
            column("team", "integer"),
        ];
        m.tables.insert(t.clone(), Table { name: t.clone(), columns, comment: Some("people".into()), constraints: vec![pk, fk] });
        let mood = QualName::new("app", "mood");
        m.enums.insert(mood.clone(), Enum { name: mood, labels: vec!["sad".into(), "ok".into()], comment: None });
        let seq = QualName::new("app", "users_n_seq");
        m.sequences.insert(
            seq.clone(),
            Sequence {
                name: seq,
                data_type: "integer".into(),
                options: SequenceOptions { cycle: true, ..ints },
                owned_by: Some(OwnedBy { table: t, column: "n".into() }),
                comment: Some("counter".into()),
            },
        );
        m
    }

    #[test]
    fn model_round_trips_through_json() {
        let m = sample();
        let json = serde_json::to_string(&m).expect("serialize");
        let back: Model = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, m);
        // `Identity` equality ignores the sequence name; the JSON does not.
        assert_eq!(serde_json::to_string(&back).expect("serialize"), json);
    }

    #[test]
    fn model_lists_refuse_duplicates() {
        let s = r#"{"schemas":[{"name":"a","comment":null},{"name":"a","comment":null}],"tables":[],"enums":[],"sequences":[]}"#;
        let err = serde_json::from_str::<Model>(s).expect_err("duplicate schema");
        assert!(err.to_string().contains("listed twice"), "{err}");
    }

    /// The JSON of a model is part of pgmig's interface: a change here must be deliberate.
    #[test]
    fn model_json_is_pinned() {
        let t = QualName::new("public", "t");
        let mut m = Model::empty_database();
        let id = Column {
            name: "id".into(),
            type_name: "bigint".into(),
            type_ref: None,
            not_null: true,
            default: None,
            comment: None,
            default_sequences: vec![],
            identity: Some(Identity {
                generated: Generated::Always,
                options: SequenceOptions::defaults("bigint", 1).expect("bigint"),
                sequence: None,
            }),
        };
        let check = Constraint {
            name: "t_id_check".into(),
            kind: ConstraintKind::Check,
            definition: "CHECK ((id > 0))".into(),
            deferrable: false,
            deferred: false,
            validated: true,
            columns: vec!["id".into()],
            references: None,
            comment: None,
        };
        m.tables.insert(t.clone(), Table { name: t, columns: vec![id], comment: None, constraints: vec![check] });
        let want = r#"{
  "schemas": [
    {
      "name": "public",
      "comment": "standard public schema"
    }
  ],
  "tables": [
    {
      "name": {
        "schema": "public",
        "name": "t"
      },
      "columns": [
        {
          "name": "id",
          "type_name": "bigint",
          "type_ref": null,
          "not_null": true,
          "default": null,
          "comment": null,
          "default_sequences": [],
          "identity": {
            "generated": "always",
            "options": {
              "start": 1,
              "increment": 1,
              "min": 1,
              "max": 9223372036854775807,
              "cache": 1,
              "cycle": false
            },
            "sequence": null
          }
        }
      ],
      "comment": null,
      "constraints": [
        {
          "name": "t_id_check",
          "kind": "check",
          "definition": "CHECK ((id > 0))",
          "deferrable": false,
          "deferred": false,
          "validated": true,
          "columns": [
            "id"
          ],
          "references": null,
          "comment": null
        }
      ]
    }
  ],
  "enums": [],
  "sequences": []
}"#;
        assert_eq!(serde_json::to_string_pretty(&m).expect("serialize"), want);
    }
}
