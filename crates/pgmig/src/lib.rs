//! pgmig's library surface. Every CLI command is one function here.

use std::path::{Path, PathBuf};

pub use pgmig_conn::Conn;
pub use pgmig_diff::{Change, diff};
pub use pgmig_inspect::inspect;
pub use pgmig_model::Model;
pub use pgmig_plan::{Plan, plan};
pub use pgmig_shadow::Shadow;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: no .sql files")]
    NoSql { path: PathBuf },
    #[error("{path}: {source}")]
    Load { path: PathBuf, source: pgmig_shadow::Error },
    #[error(transparent)]
    Conn(#[from] pgmig_conn::Error),
    #[error(transparent)]
    Shadow(#[from] pgmig_shadow::Error),
    #[error(transparent)]
    Inspect(#[from] pgmig_inspect::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A schema state: SQL files describing it, or a live database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A `.sql` file, or a directory of them loaded in lexical order.
    Sql(PathBuf),
    /// A connection string (URL or key/value).
    Database(String),
}

impl Source {
    /// URLs and key/value connection strings are databases; anything else is a path.
    pub fn parse(s: &str) -> Source {
        if s.starts_with("postgres://") || s.starts_with("postgresql://") || s.contains('=') {
            Source::Database(s.to_owned())
        } else {
            Source::Sql(PathBuf::from(s))
        }
    }
}

/// Where desired-state SQL is executed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ShadowSpec {
    #[default]
    Pgmem,
    /// An empty real Postgres database.
    Postgres(String),
}

impl ShadowSpec {
    pub fn parse(s: &str) -> ShadowSpec {
        if s == "pgmem" { ShadowSpec::Pgmem } else { ShadowSpec::Postgres(s.to_owned()) }
    }

    pub fn open(&self) -> Result<Shadow> {
        Ok(match self {
            ShadowSpec::Pgmem => Shadow::pgmem()?,
            ShadowSpec::Postgres(dsn) => Shadow::postgres(dsn)?,
        })
    }
}

/// The model of a source: SQL is executed in a fresh shadow database, then inspected.
pub fn load_model(source: &Source, shadow: &ShadowSpec) -> Result<Model> {
    match source {
        Source::Database(dsn) => Ok(inspect(&mut Conn::connect(dsn)?)?),
        Source::Sql(path) => {
            let mut sh = shadow.open()?;
            for file in sql_files(path)? {
                let sql = std::fs::read_to_string(&file).map_err(|source| Error::Io { path: file.clone(), source })?;
                sh.load(&sql).map_err(|source| Error::Load { path: file.clone(), source })?;
            }
            Ok(inspect(sh.conn())?)
        }
    }
}

/// `schema diff`: the plan that turns `from` into `to`.
pub fn schema_diff(from: &Source, to: &Source, shadow: &ShadowSpec) -> Result<Plan> {
    let a = load_model(from, shadow)?;
    let b = load_model(to, shadow)?;
    Ok(plan(&diff(&a, &b)))
}

/// `schema inspect`: SQL that recreates the source's schema in a new database.
pub fn schema_inspect(source: &Source, shadow: &ShadowSpec) -> Result<Plan> {
    let m = load_model(source, shadow)?;
    Ok(plan(&diff(&Model::empty_database(), &m)))
}

fn sql_files(path: &Path) -> Result<Vec<PathBuf>> {
    let io = |source| Error::Io { path: path.to_owned(), source };
    if !path.is_dir() {
        return Ok(vec![path.to_owned()]);
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).map_err(io)? {
        let p = entry.map_err(io)?.path();
        if p.extension().is_some_and(|e| e == "sql") {
            files.push(p);
        }
    }
    if files.is_empty() {
        return Err(Error::NoSql { path: path.to_owned() });
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_parse() {
        assert_eq!(Source::parse("schema.sql"), Source::Sql("schema.sql".into()));
        assert_eq!(Source::parse("postgres://localhost/db"), Source::Database("postgres://localhost/db".into()));
        assert_eq!(Source::parse("host=localhost user=me"), Source::Database("host=localhost user=me".into()));
    }
}
