use merkur_core::{MerkurError, MerkurResult};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use tracing::info;

const CURRENT_VERSION: i64 = 4;

const META_DDL: &str = "
CREATE TABLE IF NOT EXISTS merkur_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// v2: full-text index over memory contents for the hybrid (BM25 x vector)
/// search path.
///
/// The virtual table keeps its own copy of `(id, content)` because external-
/// content tables require an INTEGER rowid mapping, while `memories.id` is a
/// TEXT primary key. Three triggers keep the index in sync for every write
/// path — both storage backends share this database, so triggers are the only
/// synchronization point that cannot drift. `AFTER UPDATE OF content` avoids
/// reindexing on the frequent non-content updates (`update_level`,
/// `mark_consolidated`, access bumps). Archived rows stay indexed; they are
/// filtered at query time via a join on `memories.level`.
const FTS_DDL_V2: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    id UNINDEXED,
    content,
    tokenize = 'trigram'
);

CREATE TRIGGER IF NOT EXISTS memories_fts_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts (id, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS memories_fts_ad AFTER DELETE ON memories BEGIN
    DELETE FROM memories_fts WHERE id = old.id;
END;

CREATE TRIGGER IF NOT EXISTS memories_fts_au AFTER UPDATE OF content ON memories BEGIN
    DELETE FROM memories_fts WHERE id = old.id;
    INSERT INTO memories_fts (id, content) VALUES (new.id, new.content);
END;
";

pub fn migrate(pool: &Pool<SqliteConnectionManager>) -> MerkurResult<()> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("migration: get conn: {e}")))?;

    conn.execute_batch(META_DDL)
        .map_err(|e| MerkurError::Storage(format!("migration: create meta table: {e}")))?;

    let stored = get_stored_version(&conn)?;

    // Fresh database: no version row yet. The base tables were just created by
    // the unconditional DDL in `SqliteStorage::new`, so we start from 0 and let
    // every (idempotent) migration below create its auxiliary objects.
    let version = stored.unwrap_or(0);

    if version >= CURRENT_VERSION {
        return Ok(());
    }

    info!(from = version, to = CURRENT_VERSION, "Running migrations");

    if version < 2 {
        run_v2(&conn)?;
    }
    if version < 3 {
        run_v3(&conn)?;
    }
    if version < 4 {
        run_v4(&conn)?;
    }
    // Future migrations go here:
    // if version < 5 { run_v5(&conn)?; }

    set_version(&conn, CURRENT_VERSION)?;
    info!("Migrations complete (schema v{CURRENT_VERSION})");
    Ok(())
}

/// Read the persisted schema version. `None` means the database has no
/// `schema_version` row — a brand-new database, or one created before the
/// migration framework existed. The version is only ever written via
/// [`set_version`] *after* migrations run, so a crash mid-migration leaves
/// the old version on disk and the next startup replays what it needs.
fn get_stored_version(conn: &rusqlite::Connection) -> MerkurResult<Option<i64>> {
    let result: rusqlite::Result<String> = conn.query_row(
        "SELECT value FROM merkur_meta WHERE key = 'schema_version'",
        [],
        |row| row.get(0),
    );
    match result {
        Ok(v) => v
            .parse::<i64>()
            .map(Some)
            .map_err(|e| MerkurError::Storage(format!("invalid schema_version: {e}"))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MerkurError::Storage(format!(
            "migration: read version: {e}"
        ))),
    }
}

/// v1 -> v2: create the FTS index, its sync triggers, and backfill every
/// existing row. Idempotent via `IF NOT EXISTS`; the backfill is safe because
/// the virtual table can only be empty when this migration first runs.
fn run_v2(conn: &rusqlite::Connection) -> MerkurResult<()> {
    conn.execute_batch(FTS_DDL_V2)
        .map_err(|e| MerkurError::Storage(format!("migration v2: create fts: {e}")))?;
    conn.execute_batch(
        "INSERT INTO memories_fts (id, content)
         SELECT id, content FROM memories;",
    )
    .map_err(|e| MerkurError::Storage(format!("migration v2: backfill fts: {e}")))?;
    Ok(())
}

/// v2 -> v3: logical namespaces. Pure additive: a new TEXT column with a
/// server default so existing rows fall into the `"default"` bucket without a
/// rewrite, plus an index for the hot WHERE filters.
fn run_v3(conn: &rusqlite::Connection) -> MerkurResult<()> {
    conn.execute_batch(
        "ALTER TABLE memories ADD COLUMN namespace TEXT NOT NULL DEFAULT 'default';
         CREATE INDEX IF NOT EXISTS idx_mem_namespace ON memories(namespace);",
    )
    .map_err(|e| MerkurError::Storage(format!("migration v3: namespace: {e}")))?;
    Ok(())
}

/// v3 -> v4: system-learned importance. Pure additive with a neutral 0.5
/// default so existing rows start unassessed rather than silently promoted.
fn run_v4(conn: &rusqlite::Connection) -> MerkurResult<()> {
    conn.execute_batch(
        "ALTER TABLE memories ADD COLUMN importance REAL NOT NULL DEFAULT 0.5;
         CREATE INDEX IF NOT EXISTS idx_mem_importance ON memories(importance);",
    )
    .map_err(|e| MerkurError::Storage(format!("migration v4: importance: {e}")))?;
    Ok(())
}

fn set_version(conn: &rusqlite::Connection, version: i64) -> MerkurResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO merkur_meta (key, value) VALUES ('schema_version', ?1)",
        params![version.to_string()],
    )
    .map_err(|e| MerkurError::Storage(format!("migration: set version: {e}")))?;
    Ok(())
}
