//! Cuarentena: explorar y revertir lo que la app movió a un lugar seguro.
//!
//! Cada operación de cuarentena crea `quarantine/q_<ms>/manifest.jsonl` con la
//! ruta original y la copia guardada. Este módulo lee esos manifiestos,
//! permite restaurar (mover de vuelta) y borrar definitivamente. Toda acción
//! queda en `audit.jsonl`, igual que las operaciones originales.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

fn app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WizEverything")
}

pub fn root() -> PathBuf {
    app_data().join("quarantine")
}

fn audit_path() -> PathBuf {
    app_data().join("audit.jsonl")
}

fn append_audit_at(path: &Path, v: &serde_json::Value) {
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{v}");
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Una entrada de la cuarentena.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QEntry {
    pub stamp: String,
    pub original: PathBuf,
    pub stored: PathBuf,
    pub bytes: u64,
    pub at: i64,
}

impl QEntry {
    pub fn exists(&self) -> bool {
        self.stored.exists()
    }

    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "stamp": self.stamp,
            "original": self.original.to_string_lossy(),
            "stored": self.stored.to_string_lossy(),
            "bytes": self.bytes,
            "at": self.at,
            "date": crate::volumes::fmt_date(self.at),
            "exists": self.exists(),
        })
    }
}

/// Lee todas las entradas no restauradas ni purgadas, de la más nueva a la más vieja.
pub fn scan() -> Vec<QEntry> {
    scan_in(&root())
}

fn scan_in(base: &Path) -> Vec<QEntry> {
    let Ok(rd) = fs::read_dir(base) else { return Vec::new() };
    let mut out: Vec<QEntry> = Vec::new();
    for d in rd.flatten() {
        let dir = d.path();
        if !dir.is_dir() {
            continue;
        }
        let stamp = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let fallback_at = stamp
            .strip_prefix("q_")
            .and_then(|ms| ms.parse::<u64>().ok())
            .map(|ms| (ms / 1000) as i64)
            .unwrap_or(0);
        let mpath = dir.join("manifest.jsonl");
        let Ok(f) = fs::File::open(&mpath) else { continue };
        let mut restored: Vec<String> = Vec::new();
        let mut purged: Vec<String> = Vec::new();
        let mut entries: Vec<QEntry> = Vec::new();
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            let original = v.get("original").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if original.is_empty() {
                continue;
            }
            if v.get("restored").and_then(|x| x.as_bool()).unwrap_or(false) {
                restored.push(original);
                continue;
            }
            if v.get("purged").and_then(|x| x.as_bool()).unwrap_or(false) {
                purged.push(original);
                continue;
            }
            let stored = v
                .get("quarantined")
                .and_then(|x| x.as_str())
                .map(PathBuf::from)
                .unwrap_or_default();
            entries.push(QEntry {
                stamp: stamp.clone(),
                original: PathBuf::from(original),
                stored,
                bytes: v.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0),
                at: v.get("at").and_then(|x| x.as_i64()).unwrap_or(fallback_at),
            });
        }
        out.extend(entries.into_iter().filter(|e| {
            let o = e.original.to_string_lossy().to_string();
            !restored.contains(&o) && !purged.contains(&o)
        }));
    }
    out.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.original.cmp(&b.original)));
    out
}

/// Carpeta `q_<stamp>` a la que pertenece la copia (sirve también en tests).
fn entry_dir(e: &QEntry) -> PathBuf {
    let mut p = e.stored.clone();
    while let Some(parent) = p.parent() {
        if p.file_name().and_then(|n| n.to_str()) == Some(e.stamp.as_str()) {
            return p;
        }
        p = parent.to_path_buf();
    }
    root().join(&e.stamp)
}

fn append_manifest(e: &QEntry, extra: serde_json::Value) -> Result<(), String> {
    let dir = entry_dir(e);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut v = json!({
        "original": e.original.to_string_lossy(),
        "quarantined": e.stored.to_string_lossy(),
        "bytes": e.bytes,
        "at": e.at,
    });
    if let (Some(dst), Some(src)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in src {
            dst.insert(k.clone(), val.clone());
        }
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("manifest.jsonl"))
        .map_err(|e| e.to_string())?;
    writeln!(f, "{v}").map_err(|e| e.to_string())
}

/// Devuelve el archivo a su ruta original. No sobrescribe: si ya existe algo
/// en el destino, falla para no perder nada.
pub fn restore(e: &QEntry) -> Result<(), String> {
    restore_in(e, Some(&audit_path()))
}

fn restore_in(e: &QEntry, audit: Option<&Path>) -> Result<(), String> {
    if !e.stored.exists() {
        return Err(format!("la copia guardada ya no está: {}", e.stored.display()));
    }
    if e.original.exists() {
        return Err(format!("ya existe un archivo en {}; no se sobrescribe", e.original.display()));
    }
    if let Some(p) = e.original.parent() {
        fs::create_dir_all(p).map_err(|e| format!("no se pudo crear {}: {e}", p.display()))?;
    }
    if fs::rename(&e.stored, &e.original).is_err() {
        // Distinto volumen (por ejemplo, un disco externo): copiar y borrar.
        copy_tree(&e.stored, &e.original)?;
        remove_tree(&e.stored).map_err(|err| format!("no se pudo borrar la copia: {err}"))?;
    }
    append_manifest(e, json!({ "restored": true, "restored_at": now() }))?;
    if let Some(path) = audit {
        append_audit_at(path, &json!({
            "at": now(),
            "op": "quarantine_restore",
            "original": e.original.to_string_lossy(),
            "quarantined": e.stored.to_string_lossy(),
        }));
    }
    Ok(())
}

/// Borra definitivamente la copia guardada (no toca la ruta original).
pub fn purge(e: &QEntry) -> Result<(), String> {
    purge_in(e, Some(&audit_path()))
}

fn purge_in(e: &QEntry, audit: Option<&Path>) -> Result<(), String> {
    if e.stored.exists() {
        remove_tree(&e.stored).map_err(|err| format!("no se pudo borrar {}: {err}", e.stored.display()))?;
    }
    append_manifest(e, json!({ "purged": true, "purged_at": now() }))?;
    if let Some(path) = audit {
        append_audit_at(path, &json!({
            "at": now(),
            "op": "quarantine_purge",
            "original": e.original.to_string_lossy(),
            "quarantined": e.stored.to_string_lossy(),
            "bytes": e.bytes,
        }));
    }
    Ok(())
}

/// Borra copias de cuarentena más viejas que `days`. Devuelve (archivos, bytes).
pub fn purge_older_than(days: u32) -> Result<(usize, u64), String> {
    let cutoff = now() - i64::from(days) * 86_400;
    let mut n = 0usize;
    let mut bytes = 0u64;
    for e in scan() {
        if e.at < cutoff {
            bytes += e.bytes;
            purge(&e)?;
            n += 1;
        }
    }
    Ok((n, bytes))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    if src.is_dir() {
        fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        fs::copy(src, dst).map(|_| ()).map_err(|e| e.to_string())
    }
}

fn remove_tree(p: &Path) -> std::io::Result<()> {
    if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(base: &Path, name: &str) -> QEntry {
        let stamp = "q_1700000000000".to_string();
        let dir = base.join(&stamp);
        fs::create_dir_all(&dir).unwrap();
        let original = base.join("origen").join(name);
        let stored = dir.join("copia").join(name);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"hola").unwrap();
        let e = QEntry { stamp, original, stored, bytes: 4, at: 1_700_000_000 };
        let mut f = OpenOptions::new().create(true).append(true).open(dir.join("manifest.jsonl")).unwrap();
        writeln!(f, "{}", json!({
            "original": e.original.to_string_lossy(),
            "quarantined": e.stored.to_string_lossy(),
            "bytes": 4,
        })).unwrap();
        e
    }

    #[test]
    fn scans_and_restores_to_the_original_path() {
        let base = std::env::temp_dir().join(format!("wzq-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let e = make(&base, "foto.jpg");
        let found = scan_in(&base);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].original, e.original);
        restore_in(&found[0], None).unwrap();
        assert!(e.original.exists());
        assert!(!e.stored.exists());
        assert!(scan_in(&base).is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refuses_to_overwrite_on_restore() {
        let base = std::env::temp_dir().join(format!("wzq2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("origen")).unwrap();
        let e = make(&base, "x.bin");
        fs::write(&e.original, b"nuevo").unwrap();
        assert!(restore_in(&e, None).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn purge_removes_only_the_saved_copy() {
        let base = std::env::temp_dir().join(format!("wzq3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let e = make(&base, "y.bin");
        purge_in(&e, None).unwrap();
        assert!(!e.stored.exists());
        assert!(scan_in(&base).is_empty());
        let _ = fs::remove_dir_all(&base);
    }
}
