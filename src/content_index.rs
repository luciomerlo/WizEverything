//! Índice de contenido **compartido** entre WizEverything y SortEverything.
//!
//! Ruta única, sin secretos ni permisos especiales:
//! `%LOCALAPPDATA%\WizeShared\content-index.sqlite3` (en Windows).
//! Las dos apps abren el mismo archivo con WAL; una sola construye/actualiza a
//! la vez mediante un *lease* en la tabla `lease`, y la otra lee lo que ya hay.
//!
//! El esquema es idempotente (`CREATE ... IF NOT EXISTS`): si ambas arrancan a
//! la vez, SQLite serializa la creación y no hay corrupción. El lease decide
//! quién escribe durante una indexación larga; el que no es dueño sigue
//! leyendo y puede reintentar más tarde.
//!
//! Para SortEverything: usar exactamente [`db_path`] y las mismas tablas
//! (`docs`, `lease`, `meta`). El contrato está en `docs/SHARED-CONTENT-INDEX.md`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

/// Aplicación que puede reclamar el lease.
pub const APP_WIZ: &str = "WizEverything";
pub const APP_SORT: &str = "SortEverything";
/// Un lease sin latido durante este tiempo se considera abandonado.
pub const LEASE_TTL_SECS: i64 = 300;
/// Tope compartido con SortEverything: 48.000 caracteres por documento.
pub const MAX_TEXT_CHARS: usize = 48_000;

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Ruta compartida. `WIZE_SHARED_INDEX` permite tests o un perfil portable.
pub fn db_path() -> PathBuf {
    if let Some(p) = std::env::var_os("WIZE_SHARED_INDEX") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    base.join("WizeShared").join("content-index.sqlite3")
}

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS docs (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    updated INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS docs_updated ON docs(updated);
CREATE VIRTUAL TABLE IF NOT EXISTS docs_fts USING fts5(
    path UNINDEXED,
    text,
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS docs_fts_ai AFTER INSERT ON docs BEGIN
    INSERT INTO docs_fts(path, text) VALUES (new.path, new.text);
END;
CREATE TRIGGER IF NOT EXISTS docs_fts_au AFTER UPDATE ON docs BEGIN
    DELETE FROM docs_fts WHERE path = old.path;
    INSERT INTO docs_fts(path, text) VALUES (new.path, new.text);
END;
CREATE TRIGGER IF NOT EXISTS docs_fts_ad AFTER DELETE ON docs BEGIN
    DELETE FROM docs_fts WHERE path = old.path;
END;
CREATE TABLE IF NOT EXISTS lease (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    app TEXT NOT NULL,
    pid INTEGER NOT NULL,
    heartbeat INTEGER NOT NULL
);
INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '1');
"#;

/// Abre (y crea si hace falta) la base compartida. Seguro con dos procesos.
pub fn open() -> Result<Connection, String> {
    let path = db_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let conn = Connection::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    // Bases creadas antes de FTS5: poblar una sola vez.
    let ready: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'fts_ready'", [], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?;
    if ready.as_deref() != Some("1") {
        conn.execute(
            "INSERT INTO docs_fts(path, text)
             SELECT path, text FROM docs
             WHERE NOT EXISTS (SELECT 1 FROM docs_fts f WHERE f.path = docs.path)",
            [],
        )
        .map_err(|e| e.to_string())?;
        conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES ('fts_ready', '1')", [])
            .map_err(|e| e.to_string())?;
    }
    Ok(conn)
}

static GLOBAL: std::sync::OnceLock<Option<std::sync::Mutex<Connection>>> = std::sync::OnceLock::new();

/// Conexión compartida del proceso (para indexadores que abren muchos archivos).
pub fn global() -> Option<&'static std::sync::Mutex<Connection>> {
    GLOBAL.get_or_init(|| open().ok().map(std::sync::Mutex::new)).as_ref()
}

/// Intenta quedarse con la escritura durante una indexación. `false` significa
/// que la otra app está indexando ahora mismo: se puede leer, no escribir.
pub fn claim(conn: &Connection, app: &str) -> Result<bool, String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let current: Option<(String, i64)> = tx
        .query_row("SELECT app, heartbeat FROM lease WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(|e| e.to_string())?;
    let n = now();
    let can = match &current {
        None => true,
        Some((owner, beat)) => owner == app || n - beat > LEASE_TTL_SECS,
    };
    if can {
        tx.execute(
            "INSERT INTO lease(id, app, pid, heartbeat) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET app = excluded.app, pid = excluded.pid, heartbeat = excluded.heartbeat",
            params![app, std::process::id() as i64, n],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(can)
}

/// Renueva el latido si seguimos siendo dueños.
pub fn heartbeat(conn: &Connection, app: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE lease SET heartbeat = ?1, pid = ?2 WHERE id = 1 AND app = ?3",
        params![now(), std::process::id() as i64, app],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn release(conn: &Connection, app: &str) -> Result<(), String> {
    conn.execute("DELETE FROM lease WHERE id = 1 AND app = ?1", params![app])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Estado actual del lease, para diagnóstico/UI.
pub fn lease_status(conn: &Connection) -> serde_json::Value {
    let row: Option<(String, i64, i64)> = conn
        .query_row("SELECT app, pid, heartbeat FROM lease WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()
        .ok()
        .flatten();
    match row {
        Some((app, pid, beat)) => serde_json::json!({
            "owner": app,
            "pid": pid,
            "heartbeat": beat,
            "fresh": now() - beat <= LEASE_TTL_SECS,
        }),
        None => serde_json::json!({ "owner": null }),
    }
}

/// Texto cacheado para una versión exacta del archivo.
pub fn get(conn: &Connection, path: &str, size: u64, mtime: i64) -> Option<(String, String)> {
    conn.query_row(
        "SELECT kind, text FROM docs WHERE path = ?1 AND size = ?2 AND mtime = ?3",
        params![path, size as i64, mtime],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

/// Guarda/actualiza el texto extraído. Sólo el dueño del lease debería llamar.
pub fn put(conn: &Connection, path: &str, size: u64, mtime: i64, kind: &str, text: &str) -> Result<(), String> {
    let text: String = if text.chars().count() > MAX_TEXT_CHARS {
        text.chars().take(MAX_TEXT_CHARS).collect()
    } else {
        text.to_string()
    };
    conn.execute(
        "INSERT INTO docs(path, size, mtime, kind, text, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime=excluded.mtime, kind=excluded.kind,
             text=excluded.text, updated=excluded.updated",
        params![path, size as i64, mtime, kind, text, now()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Borra textos de archivos que ya no existen (barrido de mantenimiento).
pub fn prune(conn: &Connection, missing: &[String]) -> Result<usize, String> {
    let mut n = 0;
    for p in missing {
        n += conn.execute("DELETE FROM docs WHERE path = ?1", params![p]).map_err(|e| e.to_string())?;
    }
    Ok(n)
}

/// Búsqueda directa en el índice (FTS5), sin recorrer el árbol.
/// Devuelve `(path, kind, snippet)`; el llamador decide si el archivo sigue vivo.
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<(String, String, String)>, String> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let expr = tokens.join(" AND ");
    let mut st = conn
        .prepare(
            "SELECT d.path, d.kind, snippet(docs_fts, 1, '', '', '…', 18)
             FROM docs_fts JOIN docs d ON d.path = docs_fts.path
             WHERE docs_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(params![expr, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// Borra del índice los archivos que ya no existen. `max` acota el trabajo por llamada.
pub fn prune_missing(conn: &Connection, max: usize) -> Result<usize, String> {
    let mut st = conn.prepare("SELECT path FROM docs LIMIT ?1").map_err(|e| e.to_string())?;
    let paths: Vec<String> = st
        .query_map(params![max as i64], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    drop(st);
    let mut n = 0;
    for p in paths {
        if !Path::new(&p).exists() {
            n += conn.execute("DELETE FROM docs WHERE path = ?1", params![p]).map_err(|e| e.to_string())?;
        }
    }
    Ok(n)
}

pub fn stats(conn: &Connection) -> serde_json::Value {
    let docs: i64 = conn.query_row("SELECT COUNT(*) FROM docs", [], |r| r.get(0)).unwrap_or(0);
    let bytes: i64 = conn.query_row("SELECT COALESCE(SUM(LENGTH(text)), 0) FROM docs", [], |r| r.get(0)).unwrap_or(0);
    serde_json::json!({
        "path": db_path().to_string_lossy(),
        "docs": docs,
        "text_bytes": bytes,
        "lease": lease_status(conn),
    })
}

/// Ruta para mostrar en la UI.
pub fn display_path() -> String {
    db_path().to_string_lossy().into_owned()
}

pub fn _path_is_shared(_: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> PathBuf {
        // `now()` tiene resolución de segundos y los tests corren en paralelo:
        // sin nanosegundos ambos podían compartir el mismo archivo.
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("wzshared-{}-{nanos}.sqlite3", std::process::id()))
    }

    #[test]
    fn schema_is_idempotent_and_cache_round_trips() {
        let p = temp_db();
        let _ = std::fs::remove_file(&p);
        // Todas las funciones usan db_path(); este test sólo valida SQL directo.
        let c = Connection::open(&p).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        put(&c, r"C:\a.pdf", 10, 20, "pdf", "hola mundo").unwrap();
        let (kind, text) = get(&c, "C:\\a.pdf", 10, 20).unwrap();
        assert_eq!(kind, "pdf");
        assert_eq!(text, "hola mundo");
        assert!(get(&c, "C:\\a.pdf", 11, 20).is_none());
        let found = search(&c, "mundo", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, r"C:\a.pdf");
        put(&c, r"C:\a.pdf", 10, 20, "pdf", "otro texto").unwrap();
        assert!(search(&c, "mundo", 10).unwrap().is_empty());
        assert_eq!(search(&c, "otro", 10).unwrap().len(), 1);
        let _ = claim(&c, APP_WIZ).unwrap();
        assert_eq!(lease_status(&c)["owner"], APP_WIZ);
        release(&c, APP_WIZ).unwrap();
        assert!(lease_status(&c)["owner"].is_null());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn lease_can_be_taken_when_stale() {
        let p = temp_db();
        let _ = std::fs::remove_file(&p);
        let c = Connection::open(&p).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        assert!(claim(&c, APP_SORT).unwrap());
        // La otra app no puede mientras el latido esté fresco.
        assert!(!claim(&c, APP_WIZ).unwrap());
        c.execute("UPDATE lease SET heartbeat = 0 WHERE id = 1", []).unwrap();
        assert!(claim(&c, APP_WIZ).unwrap());
        let long = "x".repeat(MAX_TEXT_CHARS + 100);
        put(&c, r"C:\long.txt", 1, 1, "text", &long).unwrap();
        let (_, stored) = get(&c, r"C:\long.txt", 1, 1).unwrap();
        assert_eq!(stored.chars().count(), MAX_TEXT_CHARS);
        let _ = std::fs::remove_file(&p);
    }
}
