//! The schema fingerprint: a hash of a [`Model`] that is the same for the same schema wherever it
//! was read from (a live database or a shadow replay; pgmem or any supported Postgres version).
//!
//! The hash is SHA-256 of the canonical text ([`canonical`]), printed as `h1:<base64>`. The
//! canonical text is written by hand rather than derived from the serde representation, so a field
//! added to the model changes the fingerprint only when this module is changed to include it, and
//! then [`FORMAT`] changes too.
//!
//! # What enters
//!
//! Exactly what the diff compares, so two models have the same fingerprint when (and, up to hash
//! collisions, only when) the diff finds nothing to change between them:
//!
//! - schemas: name, comment;
//! - enums: name, labels in order, comment;
//! - domains: name, base type, collation, default, `NOT NULL`, comment; check constraints by name,
//!   each with its definition (without `NOT VALID`), whether it is validated, and comment;
//! - composite types: name, comment; attributes by name (as columns), each with its type,
//!   collation and comment;
//! - sequences: name, data type, options, `OWNED BY`, comment;
//! - tables: name, comment; columns by name (column order is not compared: Postgres cannot reorder
//!   columns, so a diff never tries to), each with its type, `NOT NULL`, default, comment and
//!   identity (how it is generated and its sequence options); constraints by name, each with its
//!   kind, definition and comment; indexes by name, each with its definition and comment.
//!
//! Left out, because they are derived from what enters or are named by the server rather than by
//! the schema: [`Column::type_ref`] and [`Column::default_sequences`] (read from the type and the
//! default), [`Identity::sequence`] (whatever name Postgres picked), and a constraint's flags,
//! columns and references (which say nothing its definition doesn't; the referenced index name
//! is the catalog's), and an index's uniqueness and columns (its definition says them).
//!
//! Indexes, domains and composite types entered the fingerprint without a new [`FORMAT`]: a model
//! without them writes no record of them, so its fingerprint is what it was. A schema with them
//! fingerprints differently from before, as it should: earlier versions did not read them at all.
//!
//! # Encoding
//!
//! One record per line, in model order (maps are sorted by identity; columns, constraints and
//! indexes are sorted by name). A record is a tag and fields in a fixed order, separated by spaces. A string
//! is `<byte length>:<bytes>`, so any text, including spaces and newlines, is unambiguous; an
//! absent value is `-`, and a present optional value is `+` and then its fields. Integers are decimal and
//! booleans are `t` or `f`. A list is its length, then its items.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

use crate::{
    Attribute, Column, Composite, Constraint, ConstraintKind, Domain, DomainConstraint, Enum, Generated, Identity, Index, Model, OwnedBy,
    QualName, Schema, Sequence, SequenceOptions, Table,
};

/// The first line of the canonical text. Changing what enters the fingerprint, or how it is
/// written, changes this, so fingerprints of different formats never compare equal by accident.
pub const FORMAT: &str = "pgmig-fingerprint-v1";

/// The fingerprint of `model`: `h1:` and the base64 SHA-256 of [`canonical`].
pub fn fingerprint(model: &Model) -> String {
    format!("h1:{}", BASE64.encode(Sha256::digest(canonical(model).as_bytes())))
}

/// The text [`fingerprint`] hashes (see the module documentation).
///
/// Every struct is destructured with all its fields named (no `..`), so a field added to the model
/// does not compile here until it is either written or listed as excluded with the reason.
pub fn canonical(model: &Model) -> String {
    let Model { schemas, tables, enums, sequences, domains, composites } = model;
    let mut w = Writer(format!("{FORMAT}\n"));
    for s in schemas.values() {
        let Schema { name, comment } = s;
        w.tag("schema").str(name).opt(comment).end();
    }
    for e in enums.values() {
        let Enum { name, labels, comment } = e;
        w.tag("enum").name(name).list(labels).opt(comment).end();
    }
    for d in domains.values() {
        let Domain {
            name,
            base_type,
            // Derived from the base type.
            base_ref: _,
            collation,
            default,
            not_null,
            constraints,
            comment,
        } = d;
        w.tag("domain").name(name).str(base_type).opt_name(collation).opt(default).bool(*not_null).opt(comment).end();
        let mut constraints: Vec<_> = constraints.iter().collect();
        constraints.sort_by(|a, b| a.name.cmp(&b.name));
        for c in constraints {
            // The body: Postgres prints `NOT VALID` after it, pgmem does not (kaeawc/pgmem#266);
            // `validated` says it.
            let DomainConstraint { name, definition: _, validated, comment } = c;
            w.tag("domain_constraint").str(name).str(c.body()).bool(*validated).opt(comment).end();
        }
    }
    for c in composites.values() {
        let Composite { name, attributes, comment } = c;
        w.tag("composite").name(name).opt(comment).end();
        let mut attributes: Vec<_> = attributes.iter().collect();
        attributes.sort_by(|a, b| a.name.cmp(&b.name));
        for a in attributes {
            let Attribute {
                name,
                type_name,
                // Derived from the type name.
                type_ref: _,
                collation,
                comment,
            } = a;
            w.tag("attribute").str(name).str(type_name).opt_name(collation).opt(comment).end();
        }
    }
    for s in sequences.values() {
        let Sequence { name, data_type, options, owned_by, comment } = s;
        w.tag("sequence").name(name).str(data_type).options(options);
        match owned_by {
            Some(OwnedBy { table, column }) => w.raw("+").name(table).str(column),
            None => w.raw("-"),
        };
        w.opt(comment).end();
    }
    for t in tables.values() {
        let Table { name, columns, comment, constraints, indexes } = t;
        w.tag("table").name(name).opt(comment).end();
        let mut columns: Vec<_> = columns.iter().collect();
        columns.sort_by(|a, b| a.name.cmp(&b.name));
        for c in columns {
            let Column {
                name,
                type_name,
                // Derived from the type name.
                type_ref: _,
                not_null,
                default,
                comment,
                // Derived from the default expression.
                default_sequences: _,
                identity,
            } = c;
            w.tag("column").str(name).str(type_name).bool(*not_null).opt(default).opt(comment);
            match identity {
                Some(Identity {
                    generated: how,
                    options,
                    // Whatever name Postgres picked.
                    sequence: _,
                }) => w.raw("+").raw(generated(*how)).options(options),
                None => w.raw("-"),
            };
            w.end();
        }
        let mut constraints: Vec<_> = constraints.iter().collect();
        constraints.sort_by(|a, b| a.name.cmp(&b.name));
        for c in constraints {
            let Constraint {
                name,
                kind: k,
                definition,
                // The definition already says these (deferrability, validity, columns, target).
                deferrable: _,
                deferred: _,
                validated: _,
                columns: _,
                references: _,
                comment,
            } = c;
            w.tag("constraint").str(name).raw(kind(*k)).str(definition).opt(comment).end();
        }
        let mut indexes: Vec<_> = indexes.iter().collect();
        indexes.sort_by(|a, b| a.name.cmp(&b.name));
        for i in indexes {
            let Index {
                name,
                definition,
                // The definition already says these.
                unique: _,
                columns: _,
                comment,
            } = i;
            w.tag("index").str(name).str(definition).opt(comment).end();
        }
    }
    w.0
}

fn generated(g: Generated) -> &'static str {
    match g {
        Generated::Always => "always",
        Generated::ByDefault => "by_default",
    }
}

fn kind(k: ConstraintKind) -> &'static str {
    match k {
        ConstraintKind::PrimaryKey => "primary_key",
        ConstraintKind::Unique => "unique",
        ConstraintKind::Check => "check",
        ConstraintKind::ForeignKey => "foreign_key",
        ConstraintKind::Exclusion => "exclusion",
    }
}

/// Builds the canonical text: each field is written after a space, each record ends with `\n`.
struct Writer(String);

impl Writer {
    fn tag(&mut self, tag: &str) -> &mut Writer {
        self.0.push_str(tag);
        self
    }

    fn raw(&mut self, token: &str) -> &mut Writer {
        self.0.push(' ');
        self.0.push_str(token);
        self
    }

    fn str(&mut self, s: &str) -> &mut Writer {
        let _ = write!(self.0, " {}:{s}", s.len());
        self
    }

    fn opt(&mut self, s: &Option<String>) -> &mut Writer {
        match s {
            Some(s) => self.raw("+").str(s),
            None => self.raw("-"),
        }
    }

    fn opt_name(&mut self, n: &Option<QualName>) -> &mut Writer {
        match n {
            Some(n) => self.raw("+").name(n),
            None => self.raw("-"),
        }
    }

    fn int(&mut self, n: i64) -> &mut Writer {
        let _ = write!(self.0, " {n}");
        self
    }

    fn bool(&mut self, b: bool) -> &mut Writer {
        self.raw(if b { "t" } else { "f" })
    }

    fn name(&mut self, n: &QualName) -> &mut Writer {
        let QualName { schema, name } = n;
        self.str(schema).str(name)
    }

    fn list(&mut self, items: &[String]) -> &mut Writer {
        let _ = write!(self.0, " {}", items.len());
        for s in items {
            self.str(s);
        }
        self
    }

    fn options(&mut self, o: &SequenceOptions) -> &mut Writer {
        let SequenceOptions { start, increment, min, max, cache, cycle } = *o;
        self.int(start).int(increment).int(min).int(max).int(cache).bool(cycle)
    }

    fn end(&mut self) {
        self.0.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Index, References};

    fn column(name: &str, type_name: &str) -> Column {
        Column {
            name: name.into(),
            type_name: type_name.into(),
            type_ref: None,
            not_null: false,
            default: None,
            comment: None,
            default_sequences: vec![],
            identity: None,
        }
    }

    fn t() -> QualName {
        QualName::new("app", "users")
    }

    /// One of everything the fingerprint reads.
    fn sample() -> Model {
        let ints = SequenceOptions::defaults("integer", 1).expect("integer");
        let mut m = Model::empty_database();
        m.schemas.insert("app".into(), Schema { name: "app".into(), comment: Some("the app".into()) });
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
                ..column("n", "integer")
            },
            Column { type_ref: Some(QualName::new("app", "mood")), comment: Some("how they feel".into()), ..column("mood", "app.mood") },
            column("team", "integer"),
        ];
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
            definition: "FOREIGN KEY (team) REFERENCES app.users(id)".into(),
            deferrable: false,
            deferred: false,
            validated: true,
            columns: vec!["team".into()],
            references: Some(References { table: t(), columns: vec!["id".into()], constraint: Some("users_pkey".into()) }),
            comment: None,
        };
        let indexes = vec![
            Index {
                name: "users_team_idx".into(),
                definition: "CREATE INDEX users_team_idx ON app.users USING btree (team) WHERE (team > 0)".into(),
                unique: false,
                columns: vec!["team".into()],
                comment: Some("by team".into()),
            },
            Index {
                name: "users_n_key".into(),
                definition: "CREATE UNIQUE INDEX users_n_key ON app.users USING btree (n)".into(),
                unique: true,
                columns: vec!["n".into()],
                comment: None,
            },
        ];
        m.tables.insert(t(), Table { name: t(), columns, comment: Some("people".into()), constraints: vec![pk, fk], indexes });
        let mood = QualName::new("app", "mood");
        m.enums.insert(mood.clone(), Enum { name: mood, labels: vec!["sad".into(), "ok".into()], comment: None });
        let seq = QualName::new("app", "users_n_seq");
        m.sequences.insert(
            seq.clone(),
            Sequence {
                name: seq,
                data_type: "integer".into(),
                options: ints,
                owned_by: Some(OwnedBy { table: t(), column: "n".into() }),
                comment: None,
            },
        );
        m
    }

    /// A change made to a model, and what it changes.
    type Edit = fn(&mut Model);

    fn table(m: &mut Model) -> &mut Table {
        m.tables.get_mut(&t()).expect("table")
    }

    fn col<'a>(m: &'a mut Model, name: &str) -> &'a mut Column {
        table(m).columns.iter_mut().find(|c| c.name == name).expect("column")
    }

    #[test]
    fn canonical_text_is_pinned() {
        let want = "pgmig-fingerprint-v1
schema 3:app + 7:the app
schema 6:public + 22:standard public schema
enum 3:app 4:mood 2 3:sad 2:ok -
sequence 3:app 11:users_n_seq 7:integer 1 1 1 2147483647 1 f + 3:app 5:users 1:n -
table 3:app 5:users + 6:people
column 2:id 7:integer t - - + by_default 1 1 1 2147483647 1 f
column 4:mood 8:app.mood f - + 13:how they feel -
column 1:n 7:integer f + 36:nextval('app.users_n_seq'::regclass) - -
column 4:team 7:integer f - - -
constraint 10:users_pkey primary_key 16:PRIMARY KEY (id) + 7:the key
constraint 15:users_team_fkey foreign_key 43:FOREIGN KEY (team) REFERENCES app.users(id) -
index 11:users_n_key 60:CREATE UNIQUE INDEX users_n_key ON app.users USING btree (n) -
index 14:users_team_idx 76:CREATE INDEX users_team_idx ON app.users USING btree (team) WHERE (team > 0) + 7:by team
";
        assert_eq!(canonical(&sample()), want);
    }

    /// Indexes came without a new format: a model without them keeps the fingerprint it had.
    #[test]
    fn a_model_without_indexes_keeps_its_fingerprint() {
        let mut m = sample();
        table(&mut m).indexes.clear();
        assert!(!canonical(&m).contains("\nindex "));
        assert_eq!(fingerprint(&m), "h1:5sXCRUfyLpra3Bf30ZJnHjIRFWDat5XAQWp/AOGtM0M=");
    }

    #[test]
    fn fingerprint_is_h1_base64_sha256() {
        let fp = fingerprint(&Model::default());
        // SHA-256 of "pgmig-fingerprint-v1\n".
        let want = format!("h1:{}", BASE64.encode(Sha256::digest(b"pgmig-fingerprint-v1\n")));
        assert_eq!(fp, want);
        assert_eq!(fp.len(), 3 + 44);
    }

    #[test]
    fn column_and_constraint_order_are_ignored() {
        let base = fingerprint(&sample());
        let mut m = sample();
        table(&mut m).columns.reverse();
        table(&mut m).constraints.reverse();
        table(&mut m).indexes.reverse();
        assert_eq!(fingerprint(&m), base);
    }

    #[test]
    fn what_the_diff_ignores_is_ignored() {
        let base = fingerprint(&sample());
        let unchanged: Vec<(&str, Edit)> = vec![
            ("identity sequence name", |m| {
                col(m, "id").identity.as_mut().expect("identity").sequence = Some(QualName::new("app", "users_id_seq1"))
            }),
            ("identity sequence unknown", |m| col(m, "id").identity.as_mut().expect("identity").sequence = None),
            ("type_ref", |m| col(m, "mood").type_ref = None),
            ("default_sequences", |m| col(m, "n").default_sequences.clear()),
            ("constraint columns", |m| table(m).constraints[1].columns.clear()),
            ("index columns", |m| table(m).indexes[0].columns.clear()),
            ("index unique flag", |m| table(m).indexes[1].unique = false),
            ("constraint referenced index", |m| table(m).constraints[1].references.as_mut().expect("references").constraint = None),
        ];
        for (what, change) in unchanged {
            let mut m = sample();
            change(&mut m);
            assert_eq!(fingerprint(&m), base, "{what}");
        }
    }

    #[test]
    fn every_compared_field_changes_the_fingerprint() {
        let changes: Vec<(&str, Edit)> = vec![
            ("schema added", |m| {
                m.schemas.insert("b".into(), Schema { name: "b".into(), comment: None });
            }),
            ("schema comment", |m| m.schemas.get_mut("app").expect("app").comment = None),
            ("public comment", |m| m.schemas.get_mut("public").expect("public").comment = Some("standard public schema ".into())),
            ("enum label", |m| m.enums.values_mut().for_each(|e| e.labels[1] = "fine".into())),
            ("enum label order", |m| m.enums.values_mut().for_each(|e| e.labels.reverse())),
            ("enum label added", |m| m.enums.values_mut().for_each(|e| e.labels.push("happy".into()))),
            ("enum comment", |m| m.enums.values_mut().for_each(|e| e.comment = Some("x".into()))),
            ("sequence type", |m| m.sequences.values_mut().for_each(|s| s.data_type = "bigint".into())),
            ("sequence start", |m| m.sequences.values_mut().for_each(|s| s.options.start = 5)),
            ("sequence increment", |m| m.sequences.values_mut().for_each(|s| s.options.increment = 2)),
            ("sequence min", |m| m.sequences.values_mut().for_each(|s| s.options.min = 0)),
            ("sequence max", |m| m.sequences.values_mut().for_each(|s| s.options.max = 100)),
            ("sequence cache", |m| m.sequences.values_mut().for_each(|s| s.options.cache = 10)),
            ("sequence cycle", |m| m.sequences.values_mut().for_each(|s| s.options.cycle = true)),
            ("sequence owner", |m| m.sequences.values_mut().for_each(|s| s.owned_by = None)),
            ("sequence owner column", |m| {
                m.sequences.values_mut().for_each(|s| s.owned_by.as_mut().expect("owned").column = "team".into())
            }),
            ("sequence comment", |m| m.sequences.values_mut().for_each(|s| s.comment = Some("x".into()))),
            ("table comment", |m| table(m).comment = None),
            ("column dropped", |m| table(m).columns.retain(|c| c.name != "team")),
            ("column renamed", |m| col(m, "team").name = "squad".into()),
            ("column type", |m| col(m, "team").type_name = "bigint".into()),
            ("column not null", |m| col(m, "team").not_null = true),
            ("column default", |m| col(m, "team").default = Some("0".into())),
            ("column default text", |m| col(m, "n").default = Some("nextval('app.users_n_seq')".into())),
            ("column comment", |m| col(m, "team").comment = Some("".into())),
            ("identity added", |m| {
                col(m, "team").identity = col(m, "id").identity.clone();
            }),
            ("identity dropped", |m| col(m, "id").identity = None),
            ("identity generated", |m| col(m, "id").identity.as_mut().expect("identity").generated = Generated::Always),
            ("identity option", |m| col(m, "id").identity.as_mut().expect("identity").options.cache = 20),
            ("constraint renamed", |m| table(m).constraints[0].name = "pk".into()),
            ("constraint kind", |m| table(m).constraints[0].kind = ConstraintKind::Unique),
            ("constraint definition", |m| table(m).constraints[1].definition.push_str(" DEFERRABLE")),
            ("constraint comment", |m| table(m).constraints[1].comment = Some("x".into())),
            ("constraint dropped", |m| table(m).constraints.truncate(1)),
            ("index renamed", |m| table(m).indexes[0].name = "users_by_team".into()),
            ("index definition", |m| table(m).indexes[0].definition = "CREATE INDEX users_team_idx ON app.users USING hash (team)".into()),
            ("index comment", |m| table(m).indexes[1].comment = Some("x".into())),
            ("index dropped", |m| table(m).indexes.truncate(1)),
            ("table renamed", |m| {
                let mut tb = m.tables.remove(&t()).expect("table");
                tb.name = QualName::new("app", "people");
                m.tables.insert(tb.name.clone(), tb);
            }),
        ];
        let base = fingerprint(&sample());
        let mut seen = std::collections::BTreeSet::from([base.clone()]);
        for (what, change) in changes {
            let mut m = sample();
            change(&mut m);
            let fp = fingerprint(&m);
            assert_ne!(fp, base, "{what}");
            assert!(seen.insert(fp), "{what}: same fingerprint as another change");
        }
    }

    #[test]
    fn strings_cannot_run_into_each_other() {
        // Without length prefixes these two tables would print the same fields.
        let one = |comment: &str, schema: &str| {
            let mut m = Model::default();
            m.schemas.insert(schema.into(), Schema { name: schema.into(), comment: Some(comment.into()) });
            fingerprint(&m)
        };
        assert_ne!(one("a b", "c"), one("a", "b c"));
        assert_ne!(one("a\nschema 1:b -", "x"), one("a", "x"));
        let empty = |comment: Option<&str>| {
            let mut m = Model::default();
            m.schemas.insert("s".into(), Schema { name: "s".into(), comment: comment.map(Into::into) });
            fingerprint(&m)
        };
        assert_ne!(empty(None), empty(Some("")));
    }

    /// [`sample`] with a domain and a composite type.
    fn with_types() -> Model {
        let mut m = sample();
        let d = QualName::new("app", "posint");
        let check = |name: &str, definition: &str, validated| DomainConstraint {
            name: name.into(),
            definition: definition.into(),
            validated,
            comment: None,
        };
        m.domains.insert(
            d.clone(),
            Domain {
                name: d.clone(),
                base_type: "integer".into(),
                base_ref: None,
                collation: None,
                default: Some("1".into()),
                not_null: true,
                constraints: vec![
                    check("posint_small", "CHECK ((VALUE < 10)) NOT VALID", false),
                    check("posint_check", "CHECK ((VALUE > 0))", true),
                ],
                comment: Some("pos".into()),
            },
        );
        let p = QualName::new("app", "pair");
        let attr = |name: &str, type_name: &str, type_ref: Option<QualName>| Attribute {
            name: name.into(),
            type_name: type_name.into(),
            type_ref,
            collation: None,
            comment: None,
        };
        m.composites.insert(
            p.clone(),
            Composite { name: p, attributes: vec![attr("b", "app.posint", Some(d)), attr("a", "text", None)], comment: None },
        );
        m
    }

    fn domain(m: &mut Model) -> &mut Domain {
        m.domains.values_mut().next().expect("domain")
    }

    fn composite(m: &mut Model) -> &mut Composite {
        m.composites.values_mut().next().expect("composite")
    }

    #[test]
    fn domains_and_composite_types_enter_the_canonical_text() {
        let text = canonical(&with_types());
        assert!(
            text.contains(
                "enum 3:app 4:mood 2 3:sad 2:ok -
domain 3:app 6:posint 7:integer - + 1:1 t + 3:pos
domain_constraint 12:posint_check 19:CHECK ((VALUE > 0)) t -
domain_constraint 12:posint_small 20:CHECK ((VALUE < 10)) f -
composite 3:app 4:pair -
attribute 1:a 4:text - -
attribute 1:b 10:app.posint - -
sequence "
            ),
            "{text}"
        );
    }

    #[test]
    fn domain_and_composite_fields_the_diff_compares_change_the_fingerprint() {
        let base = fingerprint(&with_types());
        let unchanged: Vec<(&str, Edit)> = vec![
            ("base_ref", |m| domain(m).base_ref = Some(QualName::new("x", "y"))),
            ("attribute type_ref", |m| composite(m).attributes[0].type_ref = None),
            ("attribute order", |m| composite(m).attributes.reverse()),
            ("constraint order", |m| domain(m).constraints.reverse()),
            // pgmem prints no NOT VALID; the flag says it.
            ("NOT VALID in the definition", |m| domain(m).constraints[0].definition = "CHECK ((VALUE < 10))".into()),
        ];
        for (what, change) in unchanged {
            let mut m = with_types();
            change(&mut m);
            assert_eq!(fingerprint(&m), base, "{what}");
        }
        let changes: Vec<(&str, Edit)> = vec![
            ("domain base type", |m| domain(m).base_type = "bigint".into()),
            ("domain collation", |m| domain(m).collation = Some(QualName::new("pg_catalog", "C"))),
            ("domain default", |m| domain(m).default = None),
            ("domain not null", |m| domain(m).not_null = false),
            ("domain comment", |m| domain(m).comment = None),
            ("domain check", |m| domain(m).constraints[1].definition = "CHECK ((VALUE > 1))".into()),
            ("domain check validated", |m| domain(m).constraints[0].validated = true),
            ("domain check comment", |m| domain(m).constraints[0].comment = Some("c".into())),
            ("domain check dropped", |m| domain(m).constraints.truncate(1)),
            ("composite comment", |m| composite(m).comment = Some("c".into())),
            ("attribute type", |m| composite(m).attributes[1].type_name = "varchar".into()),
            ("attribute collation", |m| composite(m).attributes[1].collation = Some(QualName::new("pg_catalog", "C"))),
            ("attribute comment", |m| composite(m).attributes[1].comment = Some("c".into())),
            ("attribute dropped", |m| composite(m).attributes.truncate(1)),
            ("composite dropped", |m| m.composites.clear()),
            ("domain dropped", |m| m.domains.clear()),
        ];
        let mut seen = std::collections::BTreeSet::from([base.clone()]);
        for (what, change) in changes {
            let mut m = with_types();
            change(&mut m);
            let fp = fingerprint(&m);
            assert_ne!(fp, base, "{what}");
            assert!(seen.insert(fp), "{what}: same fingerprint as another change");
        }
    }
}
