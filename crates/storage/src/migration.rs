use merkur_core::{MerkurError, MerkurResult};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use tracing::info;

const CURRENT_VERSION: i64 = 1;

const META_DDL: &str = "
CREATE TABLE IF NOT EXISTS merkur_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

pub fn migrate(pool: &Pool<SqliteConnectionManager>) -> MerkurResult<()> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("migration: get conn: {e}")))?;

    conn.execute_batch(META_DDL)
        .map_err(|e| MerkurError::Storage(format!("migration: create meta table: {e}")))?;

    let version = get_version(&conn)?;

    if version >= CURRENT_VERSION {
        return Ok(());
    }

    info!(from = version, to = CURRENT_VERSION, "Running migrations");

    // Future migrations go here:
    // if version < 2 { run_v2(&conn)?; }
    // if version < 3 { run_v3(&conn)?; }

    set_version(&conn, CURRENT_VERSION)?;
    info!("Migrations complete (schema v{CURRENT_VERSION})");
    Ok(())
}

fn get_version(conn: &rusqlite::Connection) -> MerkurResult<i64> {
    let result: rusqlite::Result<String> = conn.query_row(
        "SELECT value FROM merkur_meta WHERE key = 'schema_version'",
        [],
        |row| row.get(0),
    );
    match result {
        Ok(v) => v
            .parse::<i64>()
            .map_err(|e| MerkurError::Storage(format!("invalid schema_version: {e}"))),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // First run — insert initial version
            set_version(conn, CURRENT_VERSION)?;
            Ok(CURRENT_VERSION)
        }
        Err(e) => Err(MerkurError::Storage(format!(
            "migration: read version: {e}"
        ))),
    }
}

fn set_version(conn: &rusqlite::Connection, version: i64) -> MerkurResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO merkur_meta (key, value) VALUES ('schema_version', ?1)",
        params![version.to_string()],
    )
    .map_err(|e| MerkurError::Storage(format!("migration: set version: {e}")))?;
    Ok(())
}
