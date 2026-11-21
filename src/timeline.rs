//! Línea de tiempo del disco: snapshots guardados y el cambio entre cada par.
//!
//! Los snapshots son la foto compacta que ya usa "¿Qué creció?". Acá se
//! listan ordenados y se comparan dos cualesquiera para ver qué apareció, qué
//! desapareció y qué carpeta se llevó el cambio. Todo sale de los archivos
//! `.wzsnap`; no vuelve a leer el disco.

use std::path::{Path, PathBuf};

/// Cabecera de un snapshot, sin decodificar todas las entradas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub path: PathBuf,
    pub created: i64,
    pub entries: u32,
}

impl Meta {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path": self.path.to_string_lossy(),
            "name": self.path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            "created": self.created,
            "date": crate::volumes::fmt_date(self.created),
            "entries": self.entries,
        })
    }
}

/// Lista los snapshots del más viejo al más nuevo.
pub fn list() -> Vec<Meta> {
    let Ok(dir) = crate::why::snapshots_dir() else { return Vec::new() };
    list_in(&dir)
}

fn list_in(dir: &Path) -> Vec<Meta> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Meta> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("wzsnap"))
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            if bytes.len() < 20 {
                return None;
            }
            let created = i64::from_le_bytes(bytes[8..16].try_into().ok()?);
            let entries = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
            Some(Meta { path, created, entries })
        })
        .collect();
    out.sort_by(|a, b| a.created.cmp(&b.created));
    out
}

/// Compara dos snapshots y devuelve el mismo informe que "¿Qué creció?".
pub fn compare(
    t: &crate::tree::Tree,
    old: &Meta,
    new: &Meta,
    limit: usize,
    deep: bool,
) -> Result<serde_json::Value, String> {
    let a = crate::why::load(&old.path)?;
    let b = crate::why::load(&new.path)?;
    let mut v = crate::why::growth(t, &a, &b, limit, deep);
    v["kind"] = serde_json::json!("timeline");
    v["old"]["date"] = serde_json::json!(crate::volumes::fmt_date(old.created));
    v["new"]["date"] = serde_json::json!(crate::volumes::fmt_date(new.created));
    v["interval_days"] = serde_json::json!(((new.created - old.created) as f64 / 86_400.0 * 10.0).round() / 10.0);
    Ok(v)
}

/// Lista para el MCP: fechas, cantidad de entradas y, si se piden dos, el delta.
pub fn to_json(limit: usize) -> serde_json::Value {
    let frames = list();
    serde_json::json!({
        "kind": "timeline",
        "count": frames.len(),
        "frames": frames.iter().map(|m| m.to_json()).collect::<Vec<_>>(),
        "notes": [
            "Cada snapshot es una foto del índice; se guardan con 'Guardar snapshot'.",
            "Para comparar dos, usá growth con old/new o la ventana Línea de tiempo.",
        ],
        "limit": limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot;

    #[test]
    fn reads_headers_without_decoding_entries() {
        let dir = std::env::temp_dir().join(format!("wztl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let snap = snapshot::Snapshot::new(1_700_000_000, vec![]);
        std::fs::write(dir.join("a.wzsnap"), snapshot::encode(&snap)).unwrap();
        let list = list_in(&dir);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].created, 1_700_000_000);
        assert_eq!(list[0].entries, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
