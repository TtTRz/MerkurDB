use merkur_core::{MerkurError, MerkurResult};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use tracing::info;

const CURRENT_VERSION: i64 = 5;

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
    let mut conn = pool
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
        migrate_step(&mut conn, 2, run_v2)?;
    }
    if version < 3 {
        migrate_step(&mut conn, 3, run_v3)?;
    }
    if version < 4 {
        migrate_step(&mut conn, 4, run_v4)?;
    }
    if version < 5 {
        migrate_step(&mut conn, 5, run_v5)?;
    }
    // Future migrations go here:
    // if version < 6 { migrate_step(&mut conn, 6, run_v6)?; }

    info!("Migrations complete (schema v{CURRENT_VERSION})");
    Ok(())
}

/// Run one migration step and its version bump in a single transaction, so a
/// crash mid-step rolls both back and the next startup replays the step
/// cleanly. Steps must additionally stay idempotent so databases bricked by
/// the pre-transactional migrator (schema objects committed, version stale)
/// heal on boot instead of failing on duplicate objects.
fn migrate_step(
    conn: &mut rusqlite::Connection,
    to: i64,
    step: fn(&rusqlite::Connection) -> MerkurResult<()>,
) -> MerkurResult<()> {
    let tx = conn
        .transaction()
        .map_err(|e| MerkurError::Storage(format!("migration v{to}: begin tx: {e}")))?;
    step(&tx)?;
    set_version(&tx, to)?;
    tx.commit()
        .map_err(|e| MerkurError::Storage(format!("migration v{to}: commit: {e}")))?;
    Ok(())
}

/// Read the persisted schema version. `None` means the database has no
/// `schema_version` row — a brand-new database, or one created before the
/// migration framework existed. The version is written via [`set_version`]
/// inside each step's transaction, so a crash mid-migration rolls the step
/// back together with its version bump and the next startup replays it.
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
/// existing row. Idempotent via `IF NOT EXISTS` plus a `NOT IN` guard so a
/// replay (crash after the objects committed but before the version bump)
/// backfills only rows not already indexed.
fn run_v2(conn: &rusqlite::Connection) -> MerkurResult<()> {
    conn.execute_batch(FTS_DDL_V2)
        .map_err(|e| MerkurError::Storage(format!("migration v2: create fts: {e}")))?;
    conn.execute_batch(
        "INSERT INTO memories_fts (id, content)
         SELECT id, content FROM memories
         WHERE id NOT IN (SELECT id FROM memories_fts);",
    )
    .map_err(|e| MerkurError::Storage(format!("migration v2: backfill fts: {e}")))?;
    Ok(())
}

/// True when `table` already has `column`. Lets migration steps skip work a
/// previous partial run committed, healing databases bricked before the
/// migrator became transactional.
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> MerkurResult<bool> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| MerkurError::Storage(format!("migration: inspect {table}: {e}")))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| MerkurError::Storage(format!("migration: inspect {table}: {e}")))?;
    for name in names {
        if name.map_err(|e| MerkurError::Storage(format!("migration: inspect {table}: {e}")))?
            == column
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// v2 -> v3: logical namespaces. Pure additive: a new TEXT column with a
/// server default so existing rows fall into the `"default"` bucket without a
/// rewrite, plus an index for the hot WHERE filters.
fn run_v3(conn: &rusqlite::Connection) -> MerkurResult<()> {
    if !has_column(conn, "memories", "namespace")? {
        conn.execute_batch(
            "ALTER TABLE memories ADD COLUMN namespace TEXT NOT NULL DEFAULT 'default';",
        )
        .map_err(|e| MerkurError::Storage(format!("migration v3: namespace: {e}")))?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_mem_namespace ON memories(namespace);")
        .map_err(|e| MerkurError::Storage(format!("migration v3: namespace index: {e}")))?;
    Ok(())
}

/// v3 -> v4: system-learned importance. Pure additive with a neutral 0.5
/// default so existing rows start unassessed rather than silently promoted.
fn run_v4(conn: &rusqlite::Connection) -> MerkurResult<()> {
    if !has_column(conn, "memories", "importance")? {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN importance REAL NOT NULL DEFAULT 0.5;")
            .map_err(|e| MerkurError::Storage(format!("migration v4: importance: {e}")))?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_mem_importance ON memories(importance);")
        .map_err(|e| MerkurError::Storage(format!("migration v4: importance index: {e}")))?;
    Ok(())
}

/// v4 -> v5: bitemporal soft-invalidation (P1-7). `invalid_at` marks a row
/// superseded or contradicted by Consolidator adjudication — retrieval
/// channels filter it, the row stays for audit until the retention purge.
/// `valid_at` is a lazy column: backfilled from `created_at`, written on every
/// insert, but not read by any code path yet — it exists so a future
/// point-in-time query does not need another migration.
fn run_v5(conn: &rusqlite::Connection) -> MerkurResult<()> {
    if !has_column(conn, "memories", "valid_at")? {
        conn.execute_batch(
            "ALTER TABLE memories ADD COLUMN valid_at TEXT;
             UPDATE memories SET valid_at = created_at WHERE valid_at IS NULL;",
        )
        .map_err(|e| MerkurError::Storage(format!("migration v5: valid_at: {e}")))?;
    }
    if !has_column(conn, "memories", "invalid_at")? {
        conn.execute_batch(
            "ALTER TABLE memories ADD COLUMN invalid_at TEXT;
             CREATE INDEX IF NOT EXISTS idx_mem_invalid_at ON memories(invalid_at);",
        )
        .map_err(|e| MerkurError::Storage(format!("migration v5: invalid_at: {e}")))?;
    }
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
