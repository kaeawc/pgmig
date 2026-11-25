//! The shadow database: where pgmig executes SQL to learn what it means.

use pgmig_conn::Conn;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("starting pgmem: {0}")]
    Pgmem(String),
    #[error("shadow database: {0}")]
    Conn(#[from] pgmig_conn::Error),
    #[error("the shadow database must be empty, but it contains {0} user objects; pgmig wipes it after each use")]
    NotEmpty(i64),
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Shadow {
    conn: Conn,
    backend: Backend,
}

enum Backend {
    // Held so the server outlives the connection; it stops on drop.
    Pgmem { _server: pgmem::Server },
    Postgres,
}

impl Shadow {
    /// A fresh embedded pgmem instance.
    pub fn pgmem() -> Result<Shadow> {
        let server = pgmem::Server::start().map_err(|e| Error::Pgmem(e.to_string()))?;
        let conn = Conn::connect(&server.conninfo())?;
        Ok(Shadow { conn, backend: Backend::Pgmem { _server: server } })
    }

    /// A real Postgres database. It must be empty; it is wiped when the shadow is dropped.
    pub fn postgres(conninfo: &str) -> Result<Shadow> {
        let mut conn = Conn::connect(conninfo)?;
        let rows = conn.query(USER_OBJECTS)?;
        let count = rows.first().and_then(|r| r.first().cloned().flatten()).and_then(|v| v.parse().ok()).unwrap_or(0);
        if count > 0 {
            return Err(Error::NotEmpty(count));
        }
        Ok(Shadow { conn, backend: Backend::Postgres })
    }

    /// Execute SQL in the shadow database.
    pub fn load(&mut self, sql: &str) -> Result<()> {
        self.conn.batch(sql)?;
        Ok(())
    }

    pub fn conn(&mut self) -> &mut Conn {
        &mut self.conn
    }
}

impl Drop for Shadow {
    fn drop(&mut self) {
        if matches!(self.backend, Backend::Postgres) {
            // Best effort: a failure leaves objects behind, which the next `postgres()` reports.
            let _ = wipe(&mut self.conn);
        }
    }
}

fn wipe(conn: &mut Conn) -> pgmig_conn::Result<()> {
    let schemas = conn.query(&format!("SELECT n.nspname FROM pg_namespace n WHERE {USER_SCHEMA}"))?;
    let mut sql = String::new();
    for name in schemas.into_iter().filter_map(|r| r.into_iter().next().flatten()) {
        sql.push_str(&format!("DROP SCHEMA \"{}\" CASCADE;\n", name.replace('"', "\"\"")));
    }
    // Recreate `public` as a new database has it (PostgreSQL 15+).
    sql.push_str("CREATE SCHEMA IF NOT EXISTS public AUTHORIZATION pg_database_owner;\nGRANT USAGE ON SCHEMA public TO PUBLIC;\n");
    conn.batch(&sql)
}

const USER_SCHEMA: &str = "n.nspname NOT IN ('pg_catalog', 'information_schema') AND left(n.nspname, 3) <> 'pg_'";

const USER_OBJECTS: &str = "\
SELECT (SELECT count(*) FROM pg_namespace n WHERE n.nspname <> 'public' AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND left(n.nspname, 3) <> 'pg_')
     + (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND left(n.nspname, 3) <> 'pg_')
     + (SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
        WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND left(n.nspname, 3) <> 'pg_')";
