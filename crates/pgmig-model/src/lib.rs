//! The schema model: what pgmig knows about a database, as read from its catalog.
//!
//! Object bodies hold exactly what the server prints (`format_type`, `pg_get_expr`, ...), so two
//! objects are equal when Postgres would print them identically.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Model {
    pub schemas: BTreeMap<String, Schema>,
    pub tables: BTreeMap<QualName, Table>,
}

impl Model {
    /// The model of a freshly created database: only the `public` schema.
    pub fn empty_database() -> Model {
        let mut m = Model::default();
        m.schemas.insert("public".to_owned(), Schema { name: "public".to_owned() });
        m
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: QualName,
    /// In `attnum` order. Postgres can't reorder columns, so diffs match columns by name.
    pub columns: Vec<Column>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_sorted() {
        assert!(QUOTED_KEYWORDS.windows(2).all(|w| w[0] < w[1]));
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
}
