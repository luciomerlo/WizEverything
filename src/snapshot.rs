//! Módulo 4 — Análisis diferencial temporal (Snapshot Delta Diff).
//!
//! Exporta un estado ultraligero del índice (una fila por archivo, hasheando las
//! rutas para no guardar strings largos) y compara dos snapshots en O(N) para
//! responder en milisegundos qué creció, qué apareció y qué desapareció.
//!
//! Formato de archivo (little-endian, propio, sin dependencias):
//! ```text
//! magic   : b"WZSNAP1\0"         (8)
//! created : i64                  (8)  unix segs
//! count   : u32                  (4)
//! repeat count times:
//!   path_hash   u64
//!   parent_hash u64
//!   name_len    u32 + name bytes (UTF-8)
//!   logical     u64
//!   physical    u64
//!   modified    i64
//!   attributes  u32
//! ```

use std::collections::HashMap;
use std::io;
use std::path::Path;

const MAGIC: &[u8; 8] = b"WZSNAP1\0";

/// Marca una entrada de directorio (tamaño 0). Los snapshots nuevos la incluyen
/// para reconstruir rutas completas al comparar dos máquinas.
pub const ATTR_DIRECTORY: u32 = 0x10;

/// Una fila del snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path_hash: u64,
    pub parent_hash: u64,
    pub name: String,
    pub logical_size: u64,
    pub physical_size: u64,
    pub modified: i64,
    pub attributes: u32,
}

pub fn is_directory(e: &Entry) -> bool {
    e.attributes & ATTR_DIRECTORY != 0
}

fn hash_path(p: &Path) -> u64 {
    // Windows es case-insensitive: normalizamos a minúsculas.
    let s = p.to_string_lossy().to_lowercase();
    xxhash_rust::xxh3::xxh3_64(s.as_bytes())
}

impl Entry {
    /// Construye una fila a partir de metadatos ya obtenidos (Everything o Win32).
    pub fn from_path(
        path: &Path,
        logical_size: u64,
        physical_size: u64,
        modified: i64,
        attributes: u32,
    ) -> Entry {
        let parent_hash = path.parent().map(hash_path).unwrap_or(0);
        Entry {
            path_hash: hash_path(path),
            parent_hash,
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
            logical_size,
            physical_size,
            modified,
            attributes,
        }
    }
}

/// Un snapshot completo.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub created: i64,
    pub entries: Vec<Entry>,
}

impl Snapshot {
    pub fn new(created: i64, entries: Vec<Entry>) -> Self {
        Snapshot { created, entries }
    }

    /// Bytes lógicos totales del snapshot.
    pub fn total_logical(&self) -> u64 {
        self.entries.iter().map(|e| e.logical_size).sum()
    }
}

/// Serializa a formato binario compacto.
pub fn encode(snap: &Snapshot) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + snap.entries.len() * 48);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&snap.created.to_le_bytes());
    out.extend_from_slice(&(snap.entries.len() as u32).to_le_bytes());
    for e in &snap.entries {
        out.extend_from_slice(&e.path_hash.to_le_bytes());
        out.extend_from_slice(&e.parent_hash.to_le_bytes());
        let name = e.name.as_bytes();
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&e.logical_size.to_le_bytes());
        out.extend_from_slice(&e.physical_size.to_le_bytes());
        out.extend_from_slice(&e.modified.to_le_bytes());
        out.extend_from_slice(&e.attributes.to_le_bytes());
    }
    out
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "snapshot truncado"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

/// Deserializa. Valida magic, versión y límites contra datos truncados/corruptos.
pub fn decode(bytes: &[u8]) -> io::Result<Snapshot> {
    let mut c = Cursor { buf: bytes, pos: 0 };
    let magic = c.take(8)?;
    if magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "magic inválido"));
    }
    let created = c.i64()?;
    let count = c.u32()? as usize;
    // Cota defensiva: cada entrada ocupa al menos 40 bytes.
    if count > bytes.len() / 40 + 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "count absurdo"));
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let path_hash = c.u64()?;
        let parent_hash = c.u64()?;
        let name_len = c.u32()? as usize;
        let name = String::from_utf8_lossy(c.take(name_len)?).into_owned();
        let logical_size = c.u64()?;
        let physical_size = c.u64()?;
        let modified = c.i64()?;
        let attributes = c.u32()?;
        entries.push(Entry {
            path_hash,
            parent_hash,
            name,
            logical_size,
            physical_size,
            modified,
            attributes,
        });
    }
    Ok(Snapshot { created, entries })
}

/// Cambio detectado en un archivo presente en ambos snapshots.
#[derive(Clone, Debug)]
pub struct Change {
    pub path_hash: u64,
    pub name: String,
    pub old_size: u64,
    pub new_size: u64,
}

impl Change {
    pub fn delta(&self) -> i64 {
        self.new_size as i64 - self.old_size as i64
    }
}

/// Resultado de comparar dos snapshots.
#[derive(Clone, Debug, Default)]
pub struct Diff {
    /// Archivos nuevos (existen en `new`, no en `old`).
    pub added: Vec<Entry>,
    /// Archivos eliminados (existen en `old`, no en `new`).
    pub removed: Vec<Entry>,
    /// Archivos que crecieron.
    pub grown: Vec<Change>,
    /// Archivos que se achicaron.
    pub shrunk: Vec<Change>,
    /// Variación neta de bytes lógicos (new - old), global.
    pub net_bytes: i64,
    /// Variación neta de bytes por carpeta (`parent_hash`).
    pub bytes_by_parent: HashMap<u64, i64>,
}

impl Diff {
    /// Bytes netos atribuibles a una carpeta.
    pub fn net_for_parent(&self, parent_hash: u64) -> i64 {
        self.bytes_by_parent.get(&parent_hash).copied().unwrap_or(0)
    }

    /// `true` si no hubo ningún cambio.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.grown.is_empty()
            && self.shrunk.is_empty()
    }
}

/// Motor de diff O(N + M) usando un índice por `path_hash`.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Diff {
    let mut index: HashMap<u64, &Entry> = HashMap::with_capacity(old.entries.len());
    for e in &old.entries {
        index.insert(e.path_hash, e);
    }

    let mut d = Diff::default();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::with_capacity(new.entries.len());

    for ne in new.entries.iter().filter(|e| !is_directory(e)) {
        seen.insert(ne.path_hash);
        match index.get(&ne.path_hash) {
            None => {
                d.net_bytes += ne.logical_size as i64;
                *d.bytes_by_parent.entry(ne.parent_hash).or_default() += ne.logical_size as i64;
                d.added.push(ne.clone());
            }
            Some(oe) => {
                let delta = ne.logical_size as i64 - oe.logical_size as i64;
                if delta != 0 {
                    d.net_bytes += delta;
                    *d.bytes_by_parent.entry(ne.parent_hash).or_default() += delta;
                    let change = Change {
                        path_hash: ne.path_hash,
                        name: ne.name.clone(),
                        old_size: oe.logical_size,
                        new_size: ne.logical_size,
                    };
                    if delta > 0 {
                        d.grown.push(change);
                    } else {
                        d.shrunk.push(change);
                    }
                }
            }
        }
    }

    for oe in old.entries.iter().filter(|e| !is_directory(e)) {
        if !seen.contains(&oe.path_hash) {
            d.net_bytes -= oe.logical_size as i64;
            *d.bytes_by_parent.entry(oe.parent_hash).or_default() -= oe.logical_size as i64;
            d.removed.push(oe.clone());
        }
    }

    // Ordenar por magnitud para que la UI muestre primero lo relevante.
    d.added.sort_by(|a, b| b.logical_size.cmp(&a.logical_size));
    d.removed.sort_by(|a, b| b.logical_size.cmp(&a.logical_size));
    d.grown.sort_by(|a, b| b.delta().cmp(&a.delta()));
    d.shrunk.sort_by(|a, b| a.delta().cmp(&b.delta()));
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, size: u64) -> Entry {
        Entry::from_path(Path::new(path), size, size, 0, 0)
    }

    #[test]
    fn roundtrip_encode_decode() {
        let snap = Snapshot::new(
            1_700_000_000,
            vec![e("C:\\a\\x.bin", 10), e("C:\\a\\y.bin", 20)],
        );
        let bytes = encode(&snap);
        let back = decode(&bytes).unwrap();
        assert_eq!(back.created, snap.created);
        assert_eq!(back.entries, snap.entries);
        assert_eq!(back.total_logical(), 30);
    }

    #[test]
    fn decode_rejects_bad_magic_and_truncation() {
        assert!(decode(b"XXXXXXXXXXXXXXXXXXXX").is_err());
        let snap = Snapshot::new(0, vec![e("C:\\a\\z.bin", 5)]);
        let mut bytes = encode(&snap);
        bytes.truncate(bytes.len() - 3);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn directory_markers_do_not_count_as_changes() {
        let mut d = e(r"C:\p", 0);
        d.attributes = ATTR_DIRECTORY;
        let old = Snapshot::new(1, vec![d.clone()]);
        let new = Snapshot::new(2, vec![d, e(r"C:\p\a.bin", 10)]);
        let delta = diff(&old, &new);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.net_bytes, 10);
        assert!(delta.removed.is_empty());
    }

    #[test]
    fn diff_detects_all_change_kinds() {
        let old = Snapshot::new(
            1,
            vec![
                e("C:\\p\\stay.bin", 100),
                e("C:\\p\\grow.bin", 100),
                e("C:\\p\\shrink.bin", 100),
                e("C:\\p\\gone.bin", 50),
            ],
        );
        let new = Snapshot::new(
            2,
            vec![
                e("C:\\p\\stay.bin", 100),
                e("C:\\p\\grow.bin", 250),
                e("C:\\p\\shrink.bin", 40),
                e("C:\\p\\fresh.bin", 300),
            ],
        );
        let d = diff(&old, &new);

        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].name, "fresh.bin");
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.removed[0].name, "gone.bin");
        assert_eq!(d.grown.len(), 1);
        assert_eq!(d.grown[0].delta(), 150);
        assert_eq!(d.shrunk.len(), 1);
        assert_eq!(d.shrunk[0].delta(), -60);

        // Neto: +300 (fresh) -50 (gone) +150 (grow) -60 (shrink) = +340
        assert_eq!(d.net_bytes, 340);
        // Todo está en la misma carpeta C:\p -> el neto por padre coincide.
        let parent = e("C:\\p\\x", 0).parent_hash;
        assert_eq!(d.net_for_parent(parent), 340);
    }

    #[test]
    fn identical_snapshots_have_empty_diff() {
        let s = Snapshot::new(1, vec![e("C:\\q\\a", 1), e("C:\\q\\b", 2)]);
        let d = diff(&s, &s);
        assert!(d.is_empty());
        assert_eq!(d.net_bytes, 0);
    }

    #[test]
    fn parent_grouping_separate_folders() {
        let old = Snapshot::new(0, vec![]);
        let new = Snapshot::new(
            1,
            vec![e("C:\\one\\big.bin", 1000), e("C:\\two\\small.bin", 10)],
        );
        let d = diff(&old, &new);
        let p1 = e("C:\\one\\x", 0).parent_hash;
        let p2 = e("C:\\two\\x", 0).parent_hash;
        assert_eq!(d.net_for_parent(p1), 1000);
        assert_eq!(d.net_for_parent(p2), 10);
        assert_eq!(d.net_bytes, 1010);
    }
}
