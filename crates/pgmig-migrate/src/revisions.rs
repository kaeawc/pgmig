//! The revisions table: which migrations a database has had applied, and how far.
//!
//! # Table
//!
//! `<schema>.pgmig_revisions` ([`TABLE`]), in a configurable schema ([`DEFAULT_SCHEMA`], `pgmig`),
//! one row per migration file:
//!
//! | column | type | |
//! |---|---|---|
//! | `version` | `text` primary key | the file's version |
//! | `description` | `text not null` | the rest of its name |
//! | `kind` | `smallint not null` | 0 normal, 1 baseline, 2 checkpoint ([`Kind`]) |
//! | `applied` | `integer not null` | statements applied |
//! | `total` | `integer not null` | statements in the file |
//! | `executed_at` | `timestamptz not null` | when the file started (the server's `now()`) |
//! | `execution_ms` | `bigint not null` | how long it took, so far |
//! | `error` | `text` | the last error, if any, or the in-flight marker ([`Revision::start_statement`]) |
//! | `error_stmt` | `text` | the statement that failed |
//! | `hash` | `text not null` | the file's own hash ([`crate::File::hash`]) |
//! | `partial_hashes` | `text[] not null` | hash of each applied statement, in order, for resuming |
//! | `pgmig_version` | `text not null` | the pgmig that wrote the row |
//! | `row_hash` | `text not null` | [`Revision::row_hash`], to catch hand edits |
//!
//! # Format
//!
//! The table's comment is `pgmig revisions table, format <n>` ([`FORMAT`]). [`Table::ensure`]
//! creates the schema, the table and the comment when the table does not exist, and otherwise only
//! checks the comment: a newer format than this pgmig knows is refused
//! ([`Error::NewerFormat`]), as is a table of that name without the comment
//! ([`Error::NotRevisionsTable`]). An older format is where a future pgmig will upgrade the table
//! in place; format 1 is the first, so there is none yet.
//!
//! # Hand edits
//!
//! Each row carries a SHA-256 over its other columns except `executed_at` (whose text depends on
//! the session's `TimeZone` and `DateStyle`); [`Table::read`] recomputes it and reports the first
//! row that does not match as [`Error::Edited`]. This catches an `UPDATE` or `INSERT` made by hand
//! or by another tool; it is not a defence against someone who recomputes the hash. Fixing the
//! table by hand is `migrate set`'s job, which rewrites the rows it touches.
//!
//! # SQL
//!
//! Every statement is plain SQL over the simple protocol: identifiers go through
//! [`quote_ident`], values through `pgmig_model::quote_literal` (an escape string `E'...'` when there is a backslash, quotes
//! doubled), which reads the same whatever `standard_conforming_strings` is set to on the target. Statements run in the caller's transaction, if any: the
//! apply engine decides what is atomic.

use std::cmp::Ordering;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use pgmig_conn::Conn;
use pgmig_model::{quote_ident, quote_literal as lit};
use sha2::{Digest, Sha256};

use crate::compare_versions;

/// The schema the table lives in unless configured otherwise.
pub const DEFAULT_SCHEMA: &str = "pgmig";
/// The table's name.
pub const TABLE: &str = "pgmig_revisions";
/// The table format this pgmig writes and reads.
pub const FORMAT: u32 = 1;

/// Key of the advisory lock that serializes creating the table.
const ENSURE_LOCK_KEY: i64 = 0x7067_6d69_6772_6576;

const COMMENT_PREFIX: &str = "pgmig revisions table, format ";

/// How `error` starts while a statement runs without a transaction ([`Revision::start_statement`]).
const IN_FLIGHT_PREFIX: &str = "in flight: statement ";

/// The columns, in the order [`Table::read`] selects them.
const COLUMNS: &str = "version, description, kind, applied, total, executed_at, execution_ms, error, error_stmt, hash, \
     array_to_string(partial_hashes, ','), coalesce(array_length(partial_hashes, 1), 0), pgmig_version, row_hash";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Conn(#[from] pgmig_conn::Error),
    #[error("{table} exists but is not a pgmig revisions table (its comment is not `{COMMENT_PREFIX}<n>`)")]
    NotRevisionsTable { table: String },
    #[error("{table} has format {found}, newer than the format {supported} this pgmig knows; upgrade pgmig")]
    NewerFormat { table: String, found: u32, supported: u32 },
    #[error("{table}: the row for version {version} was edited by hand (its row hash does not match); fix it with `pgmig migrate set`")]
    Edited { table: String, version: String },
    #[error("{table}: the row for version {version} has an invalid {column}: {reason}")]
    Malformed { table: String, version: String, column: &'static str, reason: String },
    #[error("{table}: no row for version {version}")]
    NotFound { table: String, version: String },
    #[error("revision {version}: {reason}")]
    Invalid { version: String, reason: &'static str },
}

pub type Result<T> = std::result::Result<T, Error>;

/// What a row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// A migration that was run.
    #[default]
    Normal,
    /// The state of an existing database, marked applied without running it.
    Baseline,
    /// A file holding the full state at its version.
    Checkpoint,
}

impl Kind {
    /// The value stored in `kind`.
    pub fn code(self) -> i16 {
        match self {
            Kind::Normal => 0,
            Kind::Baseline => 1,
            Kind::Checkpoint => 2,
        }
    }

    pub fn from_code(code: i16) -> Option<Kind> {
        match code {
            0 => Some(Kind::Normal),
            1 => Some(Kind::Baseline),
            2 => Some(Kind::Checkpoint),
            _ => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Normal => "normal",
            Kind::Baseline => "baseline",
            Kind::Checkpoint => "checkpoint",
        })
    }
}

/// One row of the revisions table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub version: String,
    pub description: String,
    pub kind: Kind,
    /// Statements applied so far.
    pub applied: u32,
    /// Statements in the file.
    pub total: u32,
    /// When the file started, as the server prints a `timestamptz`. `None` on a revision not yet
    /// written: [`Table::insert`] then stores the server's `now()`, and [`Table::update`] leaves
    /// the column alone.
    pub executed_at: Option<String>,
    pub execution_ms: u64,
    pub error: Option<String>,
    pub error_stmt: Option<String>,
    /// The file's own hash ([`crate::File::hash`], base64): its name and bytes, not chained.
    pub hash: String,
    /// The hash of each applied statement, in order.
    pub partial_hashes: Vec<String>,
    /// The pgmig that wrote the row.
    pub pgmig_version: String,
}

impl Revision {
    /// A revision about to start: nothing applied yet, written by this pgmig.
    pub fn start(version: &str, description: &str, kind: Kind, total: u32, hash: &str) -> Revision {
        Revision {
            version: version.to_owned(),
            description: description.to_owned(),
            kind,
            applied: 0,
            total,
            executed_at: None,
            execution_ms: 0,
            error: None,
            error_stmt: None,
            hash: hash.to_owned(),
            partial_hashes: Vec::new(),
            pgmig_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    /// One more statement applied, whose hash is `statement_hash`; clears a previous error.
    pub fn progress(&mut self, statement_hash: &str) {
        self.applied += 1;
        self.partial_hashes.push(statement_hash.to_owned());
        self.error = None;
        self.error_stmt = None;
    }

    /// Statement `index` (0-based) is about to run without a transaction. Written before it runs,
    /// the row says so until [`Revision::progress`] or [`Revision::fail`] replaces it: a row still
    /// in flight means a run stopped without knowing whether the statement took effect
    /// ([`Revision::in_flight`]).
    pub fn start_statement(&mut self, index: usize, statement: &str) {
        self.error = Some(format!("{IN_FLIGHT_PREFIX}{} started without a transaction; whether it finished was not recorded", index + 1));
        self.error_stmt = Some(statement.to_owned());
    }

    /// The 0-based index of the statement [`Revision::start_statement`] marked, while the row still
    /// says so.
    pub fn in_flight(&self) -> Option<usize> {
        let rest = self.error.as_deref()?.strip_prefix(IN_FLIGHT_PREFIX)?;
        rest.split(' ').next()?.parse::<usize>().ok()?.checked_sub(1)
    }

    /// The statement `statement` failed with `error`.
    pub fn fail(&mut self, error: &str, statement: &str) {
        self.error = Some(error.to_owned());
        self.error_stmt = Some(statement.to_owned());
    }

    /// Whether every statement is applied without an error.
    pub fn is_complete(&self) -> bool {
        self.applied == self.total && self.error.is_none()
    }

    /// SHA-256 (base64) over every column but `executed_at` and `row_hash` itself: the format, then
    /// each value as a presence byte (0 for NULL) and, when present, its length (8 bytes, big
    /// endian) and UTF-8 bytes; `partial_hashes` is its element count followed by each element.
    pub fn row_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(format!("pgmig_revisions/{FORMAT}").as_bytes());
        let mut field = |v: Option<&str>| match v {
            None => h.update([0]),
            Some(s) => {
                h.update([1]);
                h.update((s.len() as u64).to_be_bytes());
                h.update(s.as_bytes());
            }
        };
        field(Some(&self.version));
        field(Some(&self.description));
        field(Some(&self.kind.code().to_string()));
        field(Some(&self.applied.to_string()));
        field(Some(&self.total.to_string()));
        field(Some(&self.execution_ms.to_string()));
        field(self.error.as_deref());
        field(self.error_stmt.as_deref());
        field(Some(&self.hash));
        field(Some(&self.partial_hashes.len().to_string()));
        for p in &self.partial_hashes {
            field(Some(p));
        }
        field(Some(&self.pgmig_version));
        BASE64.encode(h.finalize())
    }

    /// Refuse what the table cannot hold, or could not be read back as written.
    fn check(&self) -> Result<()> {
        let invalid = |reason| Err(Error::Invalid { version: self.version.clone(), reason });
        if self.version.is_empty() || !self.version.bytes().all(|b| b.is_ascii_digit()) {
            return invalid("the version must be digits");
        }
        if self.applied > self.total {
            return invalid("more statements applied than the file has");
        }
        if i32::try_from(self.total).is_err() || i64::try_from(self.execution_ms).is_err() {
            return invalid("a count is too large");
        }
        // Fewer is allowed: a baseline, or a row fixed up by `migrate set`, has none.
        if self.partial_hashes.len() > self.applied as usize {
            return invalid("more statement hashes than applied statements");
        }
        if self.partial_hashes.iter().any(|p| p.is_empty() || p.contains([',', '{', '}', '"', '\\']) || p.chars().any(char::is_whitespace))
        {
            return invalid("a statement hash must be a non-empty word without , { } \" \\ or spaces");
        }
        let texts = [Some(&self.version), Some(&self.description), self.error.as_ref(), self.error_stmt.as_ref(), Some(&self.hash)];
        let texts = texts.into_iter().flatten().chain([&self.pgmig_version]).chain(self.executed_at.as_ref());
        if texts.chain(&self.partial_hashes).any(|s| s.contains('\0')) {
            return invalid("Postgres text cannot hold a zero byte");
        }
        Ok(())
    }

    /// Each column but `version` and `executed_at` with its value as SQL, in table order.
    fn fields(&self) -> Vec<(&'static str, String)> {
        let opt = |v: &Option<String>| v.as_deref().map_or_else(|| "NULL".to_owned(), lit);
        let partial = if self.partial_hashes.is_empty() {
            "'{}'::text[]".to_owned()
        } else {
            format!("ARRAY[{}]::text[]", self.partial_hashes.iter().map(|p| lit(p)).collect::<Vec<_>>().join(", "))
        };
        vec![
            ("description", lit(&self.description)),
            ("kind", self.kind.code().to_string()),
            ("applied", self.applied.to_string()),
            ("total", self.total.to_string()),
            ("execution_ms", self.execution_ms.to_string()),
            ("error", opt(&self.error)),
            ("error_stmt", opt(&self.error_stmt)),
            ("hash", lit(&self.hash)),
            ("partial_hashes", partial),
            ("pgmig_version", lit(&self.pgmig_version)),
            ("row_hash", lit(&self.row_hash())),
        ]
    }

    /// `executed_at` as SQL, when this revision gives it.
    fn executed_at_sql(&self) -> Option<String> {
        self.executed_at.as_deref().map(|t| format!("{}::timestamptz", lit(t)))
    }

    /// `col = value, ...` for every column [`Table::update`] writes (never `executed_at`).
    fn assignments(&self) -> String {
        let sets: Vec<String> = self.fields().into_iter().map(|(c, v)| format!("{c} = {v}")).collect();
        sets.join(", ")
    }
}

/// A revisions table that exists and has a format this pgmig reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    schema: String,
}

impl Table {
    /// The table in `schema`, created (with the schema) when it does not exist. Idempotent.
    pub fn ensure(conn: &mut Conn, schema: &str) -> Result<Table> {
        if let Some(t) = Table::open(conn, schema)? {
            return Ok(t);
        }
        // Two processes can both see no table. Creation is serialized by a transaction-level
        // advisory lock and the check is repeated once the lock is held, so the loser finds the
        // table instead of failing with 23505/42P07. Do not call this inside a transaction.
        conn.batch(&format!("BEGIN;\nSELECT pg_advisory_xact_lock({ENSURE_LOCK_KEY})"))?;
        match Table::create_locked(conn, schema) {
            Ok(t) => {
                conn.batch("COMMIT")?;
                Ok(t)
            }
            Err(e) => {
                let _ = conn.batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn create_locked(conn: &mut Conn, schema: &str) -> Result<Table> {
        if let Some(t) = Table::open(conn, schema)? {
            return Ok(t);
        }
        let s = quote_ident(schema);
        let table = format!("{s}.{TABLE}");
        conn.batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS {s};\n\
             CREATE TABLE {table} (\n    \
                 version text PRIMARY KEY,\n    \
                 description text NOT NULL,\n    \
                 kind smallint NOT NULL,\n    \
                 applied integer NOT NULL,\n    \
                 total integer NOT NULL,\n    \
                 executed_at timestamptz NOT NULL,\n    \
                 execution_ms bigint NOT NULL,\n    \
                 error text,\n    \
                 error_stmt text,\n    \
                 hash text NOT NULL,\n    \
                 partial_hashes text[] NOT NULL,\n    \
                 pgmig_version text NOT NULL,\n    \
                 row_hash text NOT NULL\n\
             );\n\
             COMMENT ON TABLE {table} IS {}",
            lit(&format!("{COMMENT_PREFIX}{FORMAT}"))
        ))?;
        Ok(Table { schema: schema.to_owned() })
    }

    /// The table in `schema`, or `None` when there is none. Never writes.
    pub fn open(conn: &mut Conn, schema: &str) -> Result<Option<Table>> {
        let rows = conn.query(&format!(
            "SELECT obj_description(c.oid, 'pg_class') FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {} AND c.relname = {}",
            lit(schema),
            lit(TABLE)
        ))?;
        let Some(row) = rows.into_iter().next() else { return Ok(None) };
        let table = qualified(schema);
        let format = row
            .into_iter()
            .next()
            .flatten()
            .and_then(|c| c.strip_prefix(COMMENT_PREFIX).and_then(|n| n.parse::<u32>().ok()))
            .ok_or_else(|| Error::NotRevisionsTable { table: table.clone() })?;
        match format.cmp(&FORMAT) {
            Ordering::Greater => Err(Error::NewerFormat { table, found: format, supported: FORMAT }),
            // Upgrades from older formats go here.
            Ordering::Less | Ordering::Equal => Ok(Some(Table { schema: schema.to_owned() })),
        }
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// `schema.pgmig_revisions`, quoted as needed.
    pub fn name(&self) -> String {
        qualified(&self.schema)
    }

    /// Every row, in version order. A row whose hash does not match is [`Error::Edited`].
    pub fn read(&self, conn: &mut Conn) -> Result<Vec<Revision>> {
        let out = self.read_unchecked(conn)?;
        if let Some(r) = out.iter().find(|r| r.edited) {
            return Err(Error::Edited { table: self.name(), version: r.revision.version.clone() });
        }
        Ok(out.into_iter().map(|r| r.revision).collect())
    }

    /// Every row, in version order, each with whether it was edited by hand, instead of refusing
    /// those: for `migrate status`, which reports them, and `migrate set`, which rewrites them. A
    /// row whose columns cannot be read at all ([`Error::Malformed`]) is still an error.
    pub fn read_unchecked(&self, conn: &mut Conn) -> Result<Vec<Stored>> {
        let table = self.name();
        let rows = conn.query(&format!("SELECT {COLUMNS} FROM {table}"))?;
        let mut out = rows.into_iter().map(|r| parse_row(&table, r)).collect::<Result<Vec<_>>>()?;
        out.sort_by(|a, b| {
            compare_versions(&a.revision.version, &b.revision.version).then_with(|| a.revision.version.cmp(&b.revision.version))
        });
        Ok(out)
    }

    /// Every row's version in the order the rows ran: by `executed_at` as the server compares it
    /// (not its text, which changes offset across daylight saving time), then by version.
    pub fn versions_by_execution(&self, conn: &mut Conn) -> Result<Vec<String>> {
        let rows = conn.query(&format!("SELECT version FROM {} ORDER BY executed_at, version", self.name()))?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
    }

    /// Add `rev`'s row; `executed_at` is the server's `now()` unless `rev` gives it. A version
    /// already in the table is the server's unique violation (23505).
    pub fn insert(&self, conn: &mut Conn, rev: &Revision) -> Result<()> {
        rev.check()?;
        let mut cols = vec!["version", "executed_at"];
        let mut vals = vec![lit(&rev.version), rev.executed_at_sql().unwrap_or_else(|| "now()".to_owned())];
        for (c, v) in rev.fields() {
            cols.push(c);
            vals.push(v);
        }
        conn.batch(&format!("INSERT INTO {} ({}) VALUES ({})", self.name(), cols.join(", "), vals.join(", ")))?;
        Ok(())
    }

    /// Replace the row for `rev.version` with `rev` (progress, finish, error). `executed_at` is
    /// never written: the row keeps the time it was inserted with.
    pub fn update(&self, conn: &mut Conn, rev: &Revision) -> Result<()> {
        rev.check()?;
        let rows = conn.query(&format!(
            "UPDATE {} SET {} WHERE version = {} RETURNING version",
            self.name(),
            rev.assignments(),
            lit(&rev.version)
        ))?;
        if rows.is_empty() {
            return Err(Error::NotFound { table: self.name(), version: rev.version.clone() });
        }
        Ok(())
    }

    /// Replace the row for `rev.version` (if any) with `rev`, `executed_at` as for
    /// [`Table::insert`], in one round trip. Outside a transaction (`in_tx` false) the delete and
    /// the insert are wrapped in one, so the row is never missing, even if the session dies
    /// between them; inside one (`in_tx` true) they are part of it.
    pub fn replace(&self, conn: &mut Conn, rev: &Revision, in_tx: bool) -> Result<()> {
        let sql = self.replace_sql(rev, in_tx)?;
        conn.batch(&sql).map_err(|e| {
            if !in_tx {
                let _ = conn.batch("ROLLBACK");
            }
            Error::Conn(e)
        })
    }

    fn replace_sql(&self, rev: &Revision, in_tx: bool) -> Result<String> {
        rev.check()?;
        let mut cols = vec!["version", "executed_at"];
        let mut vals = vec![lit(&rev.version), rev.executed_at_sql().unwrap_or_else(|| "now()".to_owned())];
        for (c, v) in rev.fields() {
            cols.push(c);
            vals.push(v);
        }
        let table = self.name();
        let body = format!(
            "DELETE FROM {table} WHERE version = {};\nINSERT INTO {table} ({}) VALUES ({})",
            lit(&rev.version),
            cols.join(", "),
            vals.join(", ")
        );
        Ok(if in_tx { body } else { format!("BEGIN;\n{body};\nCOMMIT") })
    }

    /// Remove the row for `version`; whether there was one.
    pub fn delete(&self, conn: &mut Conn, version: &str) -> Result<bool> {
        let rows = conn.query(&format!("DELETE FROM {} WHERE version = {} RETURNING version", self.name(), lit(version)))?;
        Ok(!rows.is_empty())
    }
}

fn qualified(schema: &str) -> String {
    format!("{}.{TABLE}", quote_ident(schema))
}

/// A row as stored, and whether it was edited by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub revision: Revision,
    /// Its row hash does not match ([`Revision::row_hash`]), or its statement hashes do not read
    /// back as pgmig writes them: something other than pgmig changed it.
    pub edited: bool,
}

/// A selected row.
fn parse_row(table: &str, row: pgmig_conn::Row) -> Result<Stored> {
    let mut cols = row.into_iter();
    let mut next = || cols.next().flatten();
    let version = next().unwrap_or_default();
    let bad = |column: &'static str, reason: String| Error::Malformed { table: table.to_owned(), version: version.clone(), column, reason };
    let num = |column: &'static str, v: Option<String>| -> Result<u64> {
        let v = v.ok_or_else(|| bad(column, "NULL".into()))?;
        v.parse::<u64>().map_err(|_| bad(column, format!("{v:?} is not a non-negative integer")))
    };
    let small = |n: u64, column: &'static str| u32::try_from(n).map_err(|_| bad(column, format!("{n} is too large")));
    let description = next().ok_or_else(|| bad("description", "NULL".into()))?;
    let kind_text = next();
    let kind = kind_text
        .as_deref()
        .and_then(|k| k.parse::<i16>().ok())
        .and_then(Kind::from_code)
        .ok_or_else(|| bad("kind", format!("{kind_text:?} is not 0 (normal), 1 (baseline) or 2 (checkpoint)")))?;
    let applied = small(num("applied", next())?, "applied")?;
    let total = small(num("total", next())?, "total")?;
    let executed_at = next();
    let execution_ms = num("execution_ms", next())?;
    let error = next();
    let error_stmt = next();
    let hash = next().ok_or_else(|| bad("hash", "NULL".into()))?;
    let partial_text = next().unwrap_or_default();
    let partial_count = num("partial_hashes", next())?;
    let pgmig_version = next().ok_or_else(|| bad("pgmig_version", "NULL".into()))?;
    let row_hash = next().unwrap_or_default();
    let partial_hashes: Vec<String> =
        if partial_text.is_empty() { Vec::new() } else { partial_text.split(',').map(str::to_owned).collect() };
    let rev = Revision {
        version,
        description,
        kind,
        applied,
        total,
        executed_at,
        execution_ms,
        error,
        error_stmt,
        hash,
        partial_hashes,
        pgmig_version,
    };
    // An element holding a comma (or an empty one) does not read back as it was stored: whatever
    // wrote it was not pgmig.
    let edited = rev.partial_hashes.len() as u64 != partial_count || rev.row_hash() != row_hash;
    Ok(Stored { revision: rev, edited })
}

#[cfg(test)]
mod tests {
    use super::*;

    type Edit = Box<dyn Fn(&mut Revision)>;

    fn rev() -> Revision {
        Revision::start("20261126103000", "create users", Kind::Normal, 2, "aGFzaA==")
    }

    #[test]
    fn kinds_round_trip_their_codes() {
        for k in [Kind::Normal, Kind::Baseline, Kind::Checkpoint] {
            assert_eq!(Kind::from_code(k.code()), Some(k));
        }
        assert_eq!(Kind::from_code(3), None);
        assert_eq!(Kind::from_code(-1), None);
    }

    #[test]
    fn row_hash_covers_every_column_but_executed_at() {
        let base = rev();
        let h = base.row_hash();
        let mut at = base.clone();
        at.executed_at = Some("2026-11-26 10:30:00+00".into());
        assert_eq!(at.row_hash(), h);
        let edits: Vec<Edit> = vec![
            Box::new(|r| r.version.push('1')),
            Box::new(|r| r.description.push('x')),
            Box::new(|r| r.kind = Kind::Baseline),
            Box::new(|r| r.applied = 1),
            Box::new(|r| r.total = 3),
            Box::new(|r| r.execution_ms = 5),
            Box::new(|r| r.error = Some(String::new())),
            Box::new(|r| r.error_stmt = Some("x".into())),
            Box::new(|r| r.hash.push('x')),
            Box::new(|r| r.partial_hashes.push("a".into())),
            Box::new(|r| r.pgmig_version.push('x')),
        ];
        for (i, edit) in edits.iter().enumerate() {
            let mut r = base.clone();
            edit(&mut r);
            assert_ne!(r.row_hash(), h, "edit {i}");
        }
        // Moving text from one column to the next changes the hash.
        let mut a = base.clone();
        a.error = Some("ab".into());
        a.error_stmt = Some(String::new());
        let mut b = base.clone();
        b.error = Some("a".into());
        b.error_stmt = Some("b".into());
        assert_ne!(a.row_hash(), b.row_hash());
    }

    #[test]
    fn progress_and_failure() {
        let mut r = rev();
        r.progress("s1");
        r.fail("boom (SQLSTATE 42P01)", "SELECT * FROM nope");
        assert!(!r.is_complete());
        r.progress("s2");
        assert_eq!((r.applied, r.partial_hashes.clone(), r.error.clone()), (2, vec!["s1".to_owned(), "s2".to_owned()], None));
        assert!(r.is_complete());
        r.check().expect("valid");
    }

    #[test]
    fn invalid_revisions_are_refused() {
        let cases: Vec<Edit> = vec![
            Box::new(|r| r.version = "v1".into()),
            Box::new(|r| r.version = String::new()),
            Box::new(|r| r.applied = 3),
            Box::new(|r| r.total = u32::MAX),
            Box::new(|r| r.partial_hashes.push("a".into())),
            Box::new(|r| r.progress("a,b")),
            Box::new(|r| r.progress("")),
            Box::new(|r| r.error = Some("a\0b".into())),
        ];
        for (i, c) in cases.iter().enumerate() {
            let mut r = rev();
            c(&mut r);
            assert!(matches!(r.check(), Err(Error::Invalid { .. })), "case {i}");
        }
    }

    #[test]
    fn in_flight_marker() {
        let mut r = rev();
        assert_eq!(r.in_flight(), None);
        r.start_statement(1, "CREATE INDEX CONCURRENTLY i ON t (c)");
        assert_eq!((r.in_flight(), r.error_stmt.as_deref()), (Some(1), Some("CREATE INDEX CONCURRENTLY i ON t (c)")));
        assert!(!r.is_complete());
        r.check().expect("valid");
        let mut done = r.clone();
        done.progress("s1");
        assert_eq!((done.in_flight(), done.error.as_deref()), (None, None));
        r.fail("boom (SQLSTATE 42P01)", "x");
        assert_eq!(r.in_flight(), None);
    }

    /// The row is replaced in one round trip, and outside a transaction in one transaction: a
    /// session that dies after the delete never leaves the file without its row.
    #[test]
    fn replace_is_one_transaction() {
        let t = Table { schema: DEFAULT_SCHEMA.into() };
        let r = rev();
        let alone = t.replace_sql(&r, false).expect("sql");
        assert!(alone.starts_with("BEGIN;\nDELETE FROM pgmig.pgmig_revisions WHERE version = '20261126103000';\nINSERT INTO"), "{alone}");
        assert!(alone.ends_with(";\nCOMMIT"), "{alone}");
        assert_eq!((alone.matches("DELETE").count(), alone.matches("INSERT").count()), (1, 1));
        let inside = t.replace_sql(&r, true).expect("sql");
        assert!(inside.starts_with("DELETE") && !inside.contains("BEGIN") && !inside.contains("COMMIT"), "{inside}");
        assert!(alone.contains(&inside));
        let mut bad = r.clone();
        bad.applied = 3;
        assert!(matches!(t.replace_sql(&bad, false), Err(Error::Invalid { .. })));
    }

    #[test]
    fn values_are_quoted() {
        let mut r = rev();
        r.description = "it's".into();
        r.fail("x", "SELECT 'a'");
        let sql = r.assignments();
        assert!(sql.contains("description = 'it''s'"), "{sql}");
        assert!(sql.contains("error_stmt = 'SELECT ''a'''"), "{sql}");
        assert!(sql.contains("partial_hashes = '{}'::text[]"), "{sql}");
        assert!(!sql.contains("executed_at"), "{sql}");
        assert_eq!(qualified("Ops"), "\"Ops\".pgmig_revisions");
    }
}
