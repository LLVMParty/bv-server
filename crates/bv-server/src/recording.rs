use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{params, Connection, OpenFlags};

use bv_wire::raw::constants::RESPONSE_MAGIC;
use bv_wire::raw::legacy::{
    is_legacy_request_payload, upgrade_request_in_place, upgrade_response_in_place,
};

use crate::cache::cache_key_for_payload;
use bv_wire::raw::request::is_binary_request_payload;

/// Path to the SQLite recording database. Empty disables recording.
/// Defaults to `~/.bv-server/recordings.db`.
pub const RECORD_DB_ENV: &str = "BV_SERVER_RECORD_DB";

/// Pre-rename name of [`RECORD_DB_ENV`], still honoured when the new name is unset.
pub const LEGACY_RECORD_DB_ENV: &str = "SMT_SERVER_RECORD_DB";

/// `PRAGMA user_version` of an up-to-date recording database.
///
/// - 0: written by smt-server; rows use the legacy magics.
/// - 1: rows use the current magics.
/// - 2: adds the nullable `backend` column (the backend that produced the
///   response; NULL for rows recorded before it existed, errors, and requests
///   rejected before reaching a backend).
const SCHEMA_VERSION: i64 = 2;

/// Maximum pairs committed in a single migration transaction.
const MAX_BATCH: usize = 256;

const INSERT_SQL: &str =
    "INSERT OR IGNORE INTO recordings(hash, request, response, backend) VALUES (?1, ?2, ?3, ?4)";

const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS recordings (
    hash     BLOB PRIMARY KEY CHECK(length(hash) = 32),
    request  BLOB NOT NULL,
    response BLOB NOT NULL,
    created  INTEGER NOT NULL DEFAULT (unixepoch()),
    backend  TEXT
) STRICT;";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MigrationReport {
    pub scanned: usize,
    pub inserted: usize,
    pub duplicates: usize,
    pub missing_response: usize,
    pub read_errors: usize,
    pub bad_hash: usize,
}

impl MigrationReport {
    pub fn skipped(&self) -> usize {
        self.missing_response + self.read_errors + self.bad_hash
    }
}

struct Pair {
    hash: [u8; 32],
    request: Vec<u8>,
    response: Vec<u8>,
    backend: Option<&'static str>,
}

impl Pair {
    fn canonical(request: &[u8], response: &[u8]) -> Self {
        let mut request = request.to_vec();
        upgrade_request_in_place(&mut request);
        let request = cache_key_for_payload(&request);
        let mut response = response.to_vec();
        upgrade_response_in_place(&mut response);
        let response = canonical_response(&response);
        Self::from_canonical(request, response)
    }

    fn from_canonical(request: Vec<u8>, response: Vec<u8>) -> Self {
        let hash = *blake3::hash(&request).as_bytes();
        Self {
            hash,
            request,
            response,
            backend: None,
        }
    }
}

struct Recorder {
    conn: Mutex<Connection>,
}

impl Recorder {
    fn insert(&self, pair: &Pair) -> io::Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| io::Error::other("recorder database mutex poisoned"))?;
        insert_pair(&conn, pair).map(|_| ()).map_err(sqlite_error)
    }
}

/// Persist a canonicalized request/response pair, noting which backend
/// produced the response when known. An existing row for the same request is
/// left unchanged.
pub fn record_binary_pair(request: &[u8], response: &[u8], backend: Option<&'static str>) {
    let Some(recorder) = recorder() else {
        return;
    };
    let mut pair = Pair::canonical(request, response);
    pair.backend = backend;
    if let Err(err) = recorder.insert(&pair) {
        eprintln!("bv-server: failed to persist recording: {err}");
    }
}

fn recorder() -> Option<&'static Recorder> {
    static RECORDER: OnceLock<Option<Recorder>> = OnceLock::new();
    RECORDER.get_or_init(init_recorder).as_ref()
}

/// Open the recording database now instead of on the first request.
///
/// Opening may import the pre-rename default database and upgrade an old
/// database's rows to the current magics, which can take a while for a large
/// corpus, so the server does it before accepting connections.
pub fn init_recording() {
    recorder();
}

fn init_recorder() -> Option<Recorder> {
    let path = recording_db_path()?;
    if default_recording_db_path().as_deref() == Some(path.as_path()) {
        let legacy = legacy_default_recording_db_path().filter(|legacy| legacy.is_file());
        if let Err(err) = legacy.map_or(Ok(()), |legacy| import_legacy_db(&legacy, &path)) {
            eprintln!("bv-server: failed to import pre-rename recordings into {path:?}: {err}");
        }
    }
    match open_db(&path) {
        Ok(conn) => Some(Recorder {
            conn: Mutex::new(conn),
        }),
        Err(err) => {
            eprintln!("bv-server: failed to open recording db {path:?}: {err}");
            None
        }
    }
}

/// If `path` does not exist yet, copy the pre-rename database `legacy`
/// (normally `~/.smt-server/recordings.db`) into place. The old
/// file is left unchanged; [`open_db`] then upgrades the copy's rows.
fn import_legacy_db(legacy: &Path, path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".import-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);

    // VACUUM INTO takes a consistent snapshot (including any WAL contents) and
    // works on a read-only connection, so the old database is never written.
    let source = Connection::open_with_flags(legacy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(sqlite_error)?;
    source
        .execute("VACUUM INTO ?1", params![tmp.to_string_lossy()])
        .map_err(sqlite_error)?;
    drop(source);

    // hard_link refuses to replace an existing file, so if another server
    // process won the race its copy (and anything it has recorded since) stays.
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            eprintln!(
                "bv-server: imported recordings from {legacy:?} into {path:?} (the old file is unchanged)"
            );
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

/// Bring a database written by an older version up to [`SCHEMA_VERSION`]
/// (see there for what each version changed).
///
/// Version 0 rows use the legacy `SMTQ`/`SMTR`/`SMT\0` magics. Their magics are
/// rewritten and their hashes recomputed, so legacy and current requests for
/// the same query share one row.
fn upgrade_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    // IMMEDIATE takes the write lock up front; re-check the version under it in
    // case another server process upgraded the database meanwhile.
    let tx = rusqlite::Transaction::new(conn, rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let legacy_rows: i64 = if version < 1 {
        tx.query_row(
            "SELECT count(*) FROM recordings WHERE substr(request, 1, 4) = CAST('SMTQ' AS BLOB)",
            [],
            |row| row.get(0),
        )?
    } else {
        0
    };
    if legacy_rows > 0 {
        eprintln!("bv-server: upgrading {legacy_rows} recording(s) to the bv-wire magics...");
        tx.execute_batch(
            &CREATE_TABLE_SQL.replace("IF NOT EXISTS recordings (", "recordings_upgraded ("),
        )?;
        {
            let mut select =
                tx.prepare("SELECT request, response, created FROM recordings ORDER BY created")?;
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO recordings_upgraded(hash, request, response, created)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut rows = select.query([])?;
            while let Some(row) = rows.next()? {
                let request: Vec<u8> = row.get(0)?;
                let response: Vec<u8> = row.get(1)?;
                let created: i64 = row.get(2)?;
                let pair = Pair::canonical(&request, &response);
                insert.execute(params![
                    &pair.hash[..],
                    &pair.request,
                    &pair.response,
                    created
                ])?;
            }
        }
        tx.execute_batch(
            "DROP TABLE recordings;
             ALTER TABLE recordings_upgraded RENAME TO recordings;",
        )?;
    }
    let has_backend: bool = tx.query_row(
        "SELECT count(*) FROM pragma_table_info('recordings') WHERE name = 'backend'",
        [],
        |row| row.get::<_, i64>(0).map(|count| count > 0),
    )?;
    if !has_backend {
        tx.execute_batch("ALTER TABLE recordings ADD COLUMN backend TEXT;")?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    if legacy_rows > 0 {
        eprintln!("bv-server: recording upgrade complete");
    }
    Ok(())
}

fn insert_pair(conn: &Connection, pair: &Pair) -> rusqlite::Result<usize> {
    conn.execute(
        INSERT_SQL,
        params![&pair.hash[..], &pair.request, &pair.response, pair.backend],
    )
}

/// Open (creating if needed) the recording database.
///
/// WAL mode allows readers and multiple server processes to share the same local
/// database. The busy timeout makes a process wait briefly for a serialized write
/// lock instead of failing immediately.
fn open_db(path: &Path) -> io::Result<Connection> {
    let mut conn = open_db_without_upgrade(path)?;
    upgrade_schema(&mut conn).map_err(sqlite_error)?;
    Ok(conn)
}

fn open_db_without_upgrade(path: &Path) -> io::Result<Connection> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path).map_err(sqlite_error)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(sqlite_error)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(sqlite_error)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(sqlite_error)?;
    conn.execute_batch(CREATE_TABLE_SQL).map_err(sqlite_error)?;
    Ok(conn)
}

fn sqlite_error(err: rusqlite::Error) -> io::Error {
    io::Error::other(err)
}

pub fn recording_db_path() -> Option<PathBuf> {
    let value = std::env::var_os(RECORD_DB_ENV).or_else(|| {
        let legacy = std::env::var_os(LEGACY_RECORD_DB_ENV)?;
        eprintln!("bv-server: {LEGACY_RECORD_DB_ENV} is deprecated; use {RECORD_DB_ENV}");
        Some(legacy)
    });
    match value {
        Some(value) if value.as_os_str().is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => default_recording_db_path(),
    }
}

pub fn default_recording_db_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".bv-server").join("recordings.db"))
}

/// The pre-rename default database, imported into
/// [`default_recording_db_path`] the first time the server starts.
pub fn legacy_default_recording_db_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".smt-server").join("recordings.db"))
}

/// The file-tree corpus written by early smt-server versions.
pub fn default_legacy_recording_tree() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".smt-server").join("requests"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            let mut out = PathBuf::from(drive);
            out.push(path);
            Some(out.into_os_string())
        })
        .map(PathBuf::from)
}

fn canonical_response(response: &[u8]) -> Vec<u8> {
    let mut out = response.to_vec();
    if out.len() >= 8 && out[..4] == RESPONSE_MAGIC {
        out[4..8].fill(0);
    }
    out
}

/// Import the default legacy file-tree corpus into the configured database.
///
/// The source is `~/.smt-server/requests`. The destination is resolved from
/// `BV_SERVER_RECORD_DB` or defaults to `~/.bv-server/recordings.db`.
pub fn migrate_default() -> io::Result<MigrationReport> {
    let source = default_legacy_recording_tree().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine default legacy recording directory",
        )
    })?;
    let db = recording_db_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "recording database disabled; pass an explicit database path",
        )
    })?;
    migrate_tree(&source, &db)
}

/// Import `*.req.bin`/`*.res.bin` pairs under `source` into the SQLite database
/// at `db`. Existing rows are left unchanged.
pub fn migrate_tree(source: &Path, db: &Path) -> io::Result<MigrationReport> {
    let mut conn = open_db(db)?;

    let mut req_paths = Vec::new();
    collect_req_files(source, &mut req_paths)?;
    req_paths.sort();

    let mut report = MigrationReport::default();
    for chunk in req_paths.chunks(MAX_BATCH) {
        let mut batch = Vec::with_capacity(chunk.len());
        for req_path in chunk {
            report.scanned += 1;
            if let Some(pair) = read_legacy_pair(req_path, &mut report) {
                batch.push(pair);
            }
        }
        if !batch.is_empty() {
            let (inserted, duplicates) =
                insert_batch_counted(&mut conn, &batch).map_err(sqlite_error)?;
            report.inserted += inserted;
            report.duplicates += duplicates;
        }
    }
    Ok(report)
}

fn read_legacy_pair(req_path: &Path, report: &mut MigrationReport) -> Option<Pair> {
    let Some(hash) = legacy_hash_from_req_path(req_path) else {
        report.bad_hash += 1;
        return None;
    };
    let res_path = req_path.with_file_name(format!("{hash}.res.bin"));
    if !res_path.is_file() {
        report.missing_response += 1;
        return None;
    }

    let request = match std::fs::read(req_path) {
        Ok(bytes) => bytes,
        Err(_) => {
            report.read_errors += 1;
            return None;
        }
    };
    let response = match std::fs::read(&res_path) {
        Ok(bytes) => bytes,
        Err(_) => {
            report.read_errors += 1;
            return None;
        }
    };

    // File names hash the request as it was recorded (legacy magics, zeroed
    // request_id); the stored pair is re-keyed after upgrading the magics.
    let mut recorded_key = request.clone();
    if (is_legacy_request_payload(&request) || is_binary_request_payload(&request))
        && recorded_key.len() >= 8
    {
        recorded_key[4..8].fill(0);
    }
    let actual_hash = blake3::hash(&recorded_key).to_hex().to_string();
    if hash != actual_hash {
        report.bad_hash += 1;
        return None;
    }

    Some(Pair::canonical(&request, &response))
}

fn legacy_hash_from_req_path(path: &Path) -> Option<&str> {
    let hash = path.file_name()?.to_str()?.strip_suffix(".req.bin")?;
    if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(hash)
    } else {
        None
    }
}

fn insert_batch_counted(conn: &mut Connection, batch: &[Pair]) -> rusqlite::Result<(usize, usize)> {
    let tx = conn.transaction()?;
    let mut inserted = 0usize;
    let mut duplicates = 0usize;
    {
        let mut stmt = tx.prepare_cached(INSERT_SQL)?;
        for pair in batch {
            match stmt.execute(params![
                &pair.hash[..],
                &pair.request,
                &pair.response,
                pair.backend
            ])? {
                0 => duplicates += 1,
                n => inserted += n,
            }
        }
    }
    tx.commit()?;
    Ok((inserted, duplicates))
}

fn collect_req_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_req_files(&path, out)?;
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".req.bin"))
        {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bv_wire::raw::{BinaryResponse, ExprBuilder};
    use rusqlite::OptionalExtension;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn read_pair(conn: &Connection, hash: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        conn.query_row(
            "SELECT request, response FROM recordings WHERE hash = ?1",
            params![hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn inserts_canonical_pair_deduped_by_request_id() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("bv-server-recording-test");
        let db = root.join("recordings.db");
        let conn = open_db(&db)?;

        let mut builder = ExprBuilder::new();
        let assertion = builder.bool_true()?;
        builder.assert(assertion)?;
        let req1 = builder.build_solve_request(1, 0, false, false)?;
        let req2 = builder.build_solve_request(2, 0, false, false)?;
        let res1 = BinaryResponse::error(1, "first")?.encode()?;
        let res2 = BinaryResponse::error(2, "second")?.encode()?;

        let pair1 = Pair::canonical(&req1, &res1);
        let hash = pair1.hash;
        assert_eq!(insert_pair(&conn, &pair1)?, 1);
        assert_eq!(insert_pair(&conn, &Pair::canonical(&req2, &res2))?, 0);

        let (stored_req, stored_res) = read_pair(&conn, &hash).expect("row present");
        assert_eq!(&stored_req[4..8], &[0, 0, 0, 0]);
        assert_eq!(&stored_res[4..8], &[0, 0, 0, 0]);
        assert!(String::from_utf8_lossy(&stored_res).contains("first"));

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM recordings", [], |row| row.get(0))?;
        assert_eq!(count, 1);

        drop(conn);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn migrates_legacy_tree_into_db() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("bv-server-migrate-test");
        let source = root.join("requests");
        let db = root.join("recordings.db");

        let mut builder = ExprBuilder::new();
        let assertion = builder.bool_true()?;
        builder.assert(assertion)?;
        let request = cache_key_for_payload(&builder.build_solve_request(7, 0, false, false)?);
        let response = canonical_response(&BinaryResponse::error(7, "legacy")?.encode()?);

        let hash = blake3::hash(&request).to_hex().to_string();
        let dir = source.join(&hash[0..2]).join(&hash[2..4]);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{hash}.req.bin")), &request)?;
        std::fs::write(dir.join(format!("{hash}.res.bin")), &response)?;

        let report = migrate_tree(&source, &db)?;
        assert_eq!(report.scanned, 1);
        assert_eq!(report.inserted, 1);
        assert_eq!(report.duplicates, 0);
        assert_eq!(report.skipped(), 0);

        let report = migrate_tree(&source, &db)?;
        assert_eq!(report.scanned, 1);
        assert_eq!(report.inserted, 0);
        assert_eq!(report.duplicates, 1);
        assert_eq!(report.skipped(), 0);

        let conn = open_db(&db)?;
        let key = *blake3::hash(&request).as_bytes();
        let (stored_req, stored_res) = read_pair(&conn, &key).expect("migrated row present");
        assert_eq!(stored_req, request);
        assert!(String::from_utf8_lossy(&stored_res).contains("legacy"));

        drop(conn);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// Create a database exactly as smt-server did: no `backend` column and
    /// `user_version` 0.
    fn create_smt_server_db(path: &Path) -> Result<Connection, Box<dyn std::error::Error>> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE recordings (
                hash     BLOB PRIMARY KEY CHECK(length(hash) = 32),
                request  BLOB NOT NULL,
                response BLOB NOT NULL,
                created  INTEGER NOT NULL DEFAULT (unixepoch())
            ) STRICT;",
        )?;
        Ok(conn)
    }

    fn backend_of(conn: &Connection, hash: &[u8]) -> Option<String> {
        conn.query_row(
            "SELECT backend FROM recordings WHERE hash = ?1",
            params![hash],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// A canonical (zeroed request_id) solve request and an error response, both
    /// with the legacy smt-server magics, as smt-server recorded them.
    fn legacy_pair(message: &str) -> Result<(Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
        use bv_wire::raw::legacy::{
            downgrade_response_in_place, LEGACY_EXPR_MAGIC, LEGACY_REQUEST_MAGIC,
        };
        let mut builder = ExprBuilder::new();
        let assertion = builder.bool_true()?;
        builder.assert(assertion)?;
        let mut request = cache_key_for_payload(&builder.build_solve_request(7, 0, false, false)?);
        request[..4].copy_from_slice(&LEGACY_REQUEST_MAGIC);
        request[32..36].copy_from_slice(&LEGACY_EXPR_MAGIC);
        let mut response = canonical_response(&BinaryResponse::error(7, message)?.encode()?);
        assert!(downgrade_response_in_place(&mut response));
        Ok((request, response))
    }

    #[test]
    fn opening_pre_rename_db_upgrades_rows_to_current_magics(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("bv-server-upgrade-test");
        let db = root.join("recordings.db");

        let (legacy_request, legacy_response) = legacy_pair("legacy")?;
        let legacy_hash = *blake3::hash(&legacy_request).as_bytes();
        let conn = create_smt_server_db(&db)?;
        conn.execute(
            "INSERT INTO recordings(hash, request, response, created) VALUES (?1, ?2, ?3, 1234)",
            params![&legacy_hash[..], &legacy_request, &legacy_response],
        )?;
        drop(conn);

        let conn = open_db(&db)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, SCHEMA_VERSION);
        assert!(read_pair(&conn, &legacy_hash).is_none());

        let expected = Pair::canonical(&legacy_request, &legacy_response);
        assert_eq!(&expected.request[..4], b"BVSQ");
        assert_eq!(&expected.request[32..36], b"BV\0\0");
        assert_eq!(&expected.response[..4], b"BVSR");
        let (stored_req, stored_res) = read_pair(&conn, &expected.hash).expect("upgraded row");
        assert_eq!(stored_req, expected.request);
        assert_eq!(stored_res, expected.response);
        let created: i64 =
            conn.query_row("SELECT created FROM recordings", [], |row| row.get(0))?;
        assert_eq!(created, 1234);
        assert_eq!(backend_of(&conn, &expected.hash), None);

        // Recording the same query from a current client hits the upgraded row.
        assert_eq!(insert_pair(&conn, &expected)?, 0);
        drop(conn);

        // Reopening does not redo the upgrade.
        let conn = open_db(&db)?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM recordings", [], |row| row.get(0))?;
        assert_eq!(count, 1);

        drop(conn);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn imports_pre_rename_db_without_modifying_it() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("bv-server-import-test");
        let legacy_db = root.join("old").join("recordings.db");
        let db = root.join("new").join("recordings.db");

        // Keep the writer open so the row is still only in the WAL file.
        let (request, response) = legacy_pair("from old db")?;
        let legacy_hash = *blake3::hash(&request).as_bytes();
        let writer = create_smt_server_db(&legacy_db)?;
        writer.execute(
            "INSERT INTO recordings(hash, request, response) VALUES (?1, ?2, ?3)",
            params![&legacy_hash[..], &request, &response],
        )?;
        let legacy_bytes = std::fs::read(&legacy_db)?;

        import_legacy_db(&legacy_db, &db)?;
        let conn = open_db(&db)?;
        let expected = Pair::canonical(&request, &response);
        assert!(read_pair(&conn, &expected.hash).is_some());
        drop(conn);

        // The old database is untouched and still has its legacy row.
        assert_eq!(std::fs::read(&legacy_db)?, legacy_bytes);
        assert!(read_pair(&writer, &legacy_hash).is_some());

        // An existing database is never replaced.
        let before = std::fs::read(&db)?;
        import_legacy_db(&legacy_db, &db)?;
        assert_eq!(std::fs::read(&db)?, before);

        drop(writer);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn records_backend_and_keeps_first_attribution() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("bv-server-backend-test");
        let conn = open_db(&root.join("recordings.db"))?;

        let mut builder = ExprBuilder::new();
        let assertion = builder.bool_true()?;
        builder.assert(assertion)?;
        let request = builder.build_solve_request(1, 0, false, false)?;
        let response =
            BinaryResponse::new(1, bv_wire::raw::Status::Sat, 0, Vec::new())?.encode()?;

        let mut pair = Pair::canonical(&request, &response);
        pair.backend = Some("z3");
        assert_eq!(insert_pair(&conn, &pair)?, 1);
        pair.backend = Some("binbit");
        assert_eq!(insert_pair(&conn, &pair)?, 0);
        assert_eq!(backend_of(&conn, &pair.hash).as_deref(), Some("z3"));

        drop(conn);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn version_1_db_gains_backend_column() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("bv-server-v1-test");
        let db = root.join("recordings.db");
        let conn = create_smt_server_db(&db)?;
        conn.pragma_update(None, "user_version", 1)?;
        drop(conn);

        let conn = open_db(&db)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, SCHEMA_VERSION);
        let (request, response) = legacy_pair("v1")?;
        let mut pair = Pair::canonical(&request, &response);
        pair.backend = Some("rumba");
        assert_eq!(insert_pair(&conn, &pair)?, 1);
        assert_eq!(backend_of(&conn, &pair.hash).as_deref(), Some("rumba"));

        drop(conn);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn migrates_legacy_magic_tree_into_db() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("bv-server-migrate-legacy-magic-test");
        let source = root.join("requests");
        let db = root.join("recordings.db");

        let (request, response) = legacy_pair("legacy magic")?;
        let hash = blake3::hash(&request).to_hex().to_string();
        std::fs::create_dir_all(&source)?;
        std::fs::write(source.join(format!("{hash}.req.bin")), &request)?;
        std::fs::write(source.join(format!("{hash}.res.bin")), &response)?;

        let report = migrate_tree(&source, &db)?;
        assert_eq!(report.inserted, 1);
        assert_eq!(report.skipped(), 0);

        let conn = open_db(&db)?;
        let expected = Pair::canonical(&request, &response);
        let (stored_req, stored_res) = read_pair(&conn, &expected.hash).expect("migrated row");
        assert_eq!(&stored_req[..4], b"BVSQ");
        assert_eq!(&stored_res[..4], b"BVSR");

        drop(conn);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn migration_reports_skipped_legacy_files() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("bv-server-migrate-skip-test");
        let source = root.join("requests");
        let db = root.join("recordings.db");
        std::fs::create_dir_all(&source)?;

        let missing_hash = "0".repeat(64);
        std::fs::write(
            source.join(format!("{missing_hash}.req.bin")),
            b"missing response",
        )?;

        let bad_hash = "1".repeat(64);
        std::fs::write(
            source.join(format!("{bad_hash}.req.bin")),
            b"bad hash request",
        )?;
        std::fs::write(source.join(format!("{bad_hash}.res.bin")), b"response")?;

        let report = migrate_tree(&source, &db)?;
        assert_eq!(report.scanned, 2);
        assert_eq!(report.inserted, 0);
        assert_eq!(report.duplicates, 0);
        assert_eq!(report.missing_response, 1);
        assert_eq!(report.bad_hash, 1);
        assert_eq!(report.read_errors, 0);

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
