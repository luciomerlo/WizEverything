//! Módulo 7 — Catálogo portable offline de unidades desconectadas (`.ecat`).
//!
//! Guarda un inventario comprimido de un volumen externo/red para poder
//! cruzarlo en segundo plano contra los archivos locales: si un archivo local
//! ya está respaldado en un disco desconectado, se puede marcar y no duplicar.
//!
//! Formato `.ecat`:
//! ```text
//! magic    : b"WZECAT1\0"        (8)
//! name_len : u32 + volume_name (UTF-8)
//! serial   : u64                 (series del volumen)
//! created  : i64                 (unix segs del snapshot)
//! pay_len  : u32
//! payload  : zstd( snapshot::encode(...) )
//! ```

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::snapshot::{self, Entry, Snapshot};

const MAGIC: &[u8; 8] = b"WZECAT1\0";
const ZSTD_LEVEL: i32 = 19; // alto: el catálogo se escribe una vez y se lee muchas
const MAX_PAYLOAD_BYTES: usize = 512 * 1024 * 1024;
const MAX_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
/// Cabecera fija posterior al nombre: serial (8) + created (8) + pay_len (4).
const TAIL_BYTES: usize = 20;

/// Catálogo de un volumen externo.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub volume_name: String,
    pub volume_serial: u64,
    pub created: i64,
    pub entries: Vec<Entry>,
    /// Índice `path_hash -> posición` (no se serializa; se reconstruye al leer).
    index: HashMap<u64, usize>,
}

impl Catalog {
    pub fn new(volume_name: impl Into<String>, volume_serial: u64, created: i64, entries: Vec<Entry>) -> Self {
        let mut c = Catalog {
            volume_name: volume_name.into(),
            volume_serial,
            created,
            entries,
            index: HashMap::new(),
        };
        c.reindex();
        c
    }

    /// Construye un catálogo a partir de un snapshot existente.
    pub fn from_snapshot(volume_name: impl Into<String>, volume_serial: u64, snap: &Snapshot) -> Self {
        Catalog::new(volume_name, volume_serial, snap.created, snap.entries.clone())
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (i, e) in self.entries.iter().enumerate() {
            self.index.insert(e.path_hash, i);
        }
    }

    /// Busca por hash de ruta (ya normalizado a minúsculas).
    pub fn lookup_hash(&self, path_hash: u64) -> Option<&Entry> {
        self.index.get(&path_hash).map(|&i| &self.entries[i])
    }

    /// `true` si la ruta local ya está respaldada en este volumen.
    pub fn contains_path(&self, local: &Path) -> bool {
        // Reutilizamos el hash de Entry calculándolo vía el mismo criterio.
        let probe = Entry::from_path(local, 0, 0, 0, 0);
        self.lookup_hash(probe.path_hash).is_some()
    }

    /// Devuelve la entrada respaldada correspondiente a una ruta local.
    pub fn backed_up_entry(&self, local: &Path) -> Option<&Entry> {
        let probe = Entry::from_path(local, 0, 0, 0, 0);
        self.lookup_hash(probe.path_hash)
    }

    pub fn total_logical(&self) -> u64 {
        self.entries.iter().map(|e| e.logical_size).sum()
    }

    /// Serializa a bytes `.ecat` comprimidos.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let raw = snapshot::encode_entries(self.created, &self.entries);
        let mut enc = zstd::Encoder::new(Vec::new(), ZSTD_LEVEL)?;
        enc.write_all(&raw)?;
        drop(raw);
        let payload = enc.finish()?;

        let name = self.volume_name.as_bytes();
        let mut out = Vec::with_capacity(32 + name.len() + payload.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&self.volume_serial.to_le_bytes());
        out.extend_from_slice(&self.created.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Deserializa. Valida magic, longitudes y datos Zstd corruptos.
    pub fn decode(bytes: &[u8]) -> io::Result<Catalog> {
        let mut r = bytes;
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "magic .ecat inválido"));
        }
        let name_len = read_u32(&mut r)? as usize;
        // El nombre va seguido de serial + created + pay_len: validamos contra lo
        // que realmente queda antes de asignar nada.
        if name_len > r.len().saturating_sub(TAIL_BYTES) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "name_len absurdo"));
        }
        let name_bytes = take(&mut r, name_len)?;
        let volume_name = match std::str::from_utf8(name_bytes) {
            Ok(s) => s.to_owned(),
            Err(_) => String::from_utf8_lossy(name_bytes).into_owned(),
        };
        let volume_serial = read_u64(&mut r)?;
        let created = read_i64(&mut r)?;
        let pay_len = read_u32(&mut r)? as usize;
        if pay_len > r.len() || pay_len > MAX_PAYLOAD_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "payload .ecat absurdo"));
        }
        // Sin copia: el payload se descomprime directamente desde el slice leído.
        let payload = &r[..pay_len];

        let mut decompressed = Vec::new();
        decompress_bounded(payload, &mut decompressed)?;
        let snap = snapshot::decode(&decompressed)?;

        Ok(Catalog::new(volume_name, volume_serial, created, snap.entries))
    }
}

/// Toma los próximos `n` bytes del slice sin copiarlos.
fn take<'a>(r: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if n > r.len() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "catálogo truncado"));
    }
    let (head, tail) = r.split_at(n);
    *r = tail;
    Ok(head)
}

/// Descomprime `payload` (Zstd) en `out` con tope estricto. Si el frame declara
/// su tamaño, se valida y reserva antes de descomprimir; si se supera el tope
/// durante la lectura se aborta sin asignar el resto.
fn decompress_bounded(payload: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
    decompress_with_cap(payload, out, MAX_DECOMPRESSED_BYTES)
}

fn decompress_with_cap(payload: &[u8], out: &mut Vec<u8>, cap: u64) -> io::Result<()> {
    let declared = zstd::zstd_safe::get_frame_content_size(payload).ok().flatten();
    if let Some(n) = declared {
        if n > cap {
            return Err(too_large(n, cap));
        }
        out.reserve_exact(n as usize);
    }
    let mut dec = zstd::Decoder::new(payload)?;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = dec.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        let len = out
            .len()
            .checked_add(n)
            .filter(|&len| len as u64 <= cap)
            .ok_or_else(|| too_large(cap.saturating_add(1), cap))?;
        out.reserve(len - out.len());
        out.extend_from_slice(&chunk[..n]);    }
}

fn too_large(n: u64, cap: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "catálogo descomprimido demasiado grande ({n} bytes); límite {} MB",
            cap / (1024 * 1024)
        ),
    )
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}

/// Cruza una lista de rutas locales contra varios catálogos cargados.
/// Devuelve, por ruta local, el volumen donde ya está respaldada (si existe).
pub fn cross_reference<'a>(
    catalogs: &'a [Catalog],
    locals: &[std::path::PathBuf],
) -> Vec<(std::path::PathBuf, Option<&'a str>)> {
    locals
        .iter()
        .map(|p| {
            let hit = catalogs
                .iter()
                .find(|c| c.contains_path(p))
                .map(|c| c.volume_name.as_str());
            (p.clone(), hit)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, size: u64) -> Entry {
        Entry::from_path(Path::new(path), size, size, 0, 0)
    }

    fn sample() -> Catalog {
        Catalog::new(
            "Backup-USB-01",
            0xDEAD_BEEF,
            1_700_000_000,
            vec![
                e("E:\\fotos\\a.jpg", 1000),
                e("E:\\fotos\\b.jpg", 2000),
                e("E:\\docs\\c.pdf", 3000),
            ],
        )
    }

    #[test]
    fn roundtrip_preserves_metadata() {
        let cat = sample();
        let bytes = cat.encode().unwrap();
        let back = Catalog::decode(&bytes).unwrap();
        assert_eq!(back.volume_name, "Backup-USB-01");
        assert_eq!(back.volume_serial, 0xDEAD_BEEF);
        assert_eq!(back.created, 1_700_000_000);
        assert_eq!(back.entries.len(), 3);
        assert_eq!(back.total_logical(), 6000);
    }

    #[test]
    fn lookup_and_contains() {
        let cat = sample();
        assert!(cat.contains_path(Path::new("E:\\fotos\\a.jpg")));
        // Case-insensitive como Windows.
        assert!(cat.contains_path(Path::new("e:\\FOTOS\\A.JPG")));
        assert!(!cat.contains_path(Path::new("E:\\fotos\\zzz.jpg")));
        let hit = cat.backed_up_entry(Path::new("E:\\docs\\c.pdf")).unwrap();
        assert_eq!(hit.logical_size, 3000);
    }

    #[test]
    fn decode_rejects_corruption() {
        assert!(Catalog::decode(b"not-an-ecat-file-at-all............").is_err());
        let mut bytes = sample().encode().unwrap();
        let n = bytes.len();
        bytes.truncate(n - 5);
        assert!(Catalog::decode(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_absurd_lengths_before_allocating() {
        // Cabecera cortada: ni siquiera hay name_len.
        assert!(Catalog::decode(MAGIC).is_err());

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&100u32.to_le_bytes()); // name_len
        bytes.extend_from_slice(&[0u8; TAIL_BYTES]); // relleno para el resto de la cabecera
        let err = Catalog::decode(&bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("name_len"));

        // pay_len por encima del límite duro: error sin reservar el payload.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&2i64.to_le_bytes());
        bytes.extend_from_slice(&((MAX_PAYLOAD_BYTES as u32) + 1).to_le_bytes());
        let err = Catalog::decode(&bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("payload"));
    }

    #[test]
    fn decompression_cap_is_enforced() {
        let raw = vec![7u8; 4096];
        // `bulk::compress` declara el tamaño del frame: se valida antes de descomprimir.
        let declared = zstd::bulk::compress(&raw, 1).unwrap();
        let mut out = Vec::new();
        let err = decompress_with_cap(&declared, &mut out, 64).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(out.is_empty(), "no debe descomprimirse nada");

        // Aun sin tamaño declarado, el tope se aplica durante el streaming.
        let mut enc = zstd::Encoder::new(Vec::new(), 1).unwrap();
        enc.write_all(&raw).unwrap();
        let streaming = enc.finish().unwrap();
        let mut out = Vec::new();
        let err = decompress_with_cap(&streaming, &mut out, 64).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(out.len() <= 64);

        // Con tope suficiente, descomprime completo.
        let mut out = Vec::new();
        decompress_with_cap(&streaming, &mut out, 4096).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn compresses_repetitive_catalog() {
        // Rutas repetitivas -> el .ecat debe pesar mucho menos que el snapshot crudo.
        let entries: Vec<Entry> = (0..2000)
            .map(|i| e(&format!("E:\\backup\\carpeta_repetitiva\\archivo_{i}.dat"), 1024))
            .collect();
        let cat = Catalog::new("USB", 1, 0, entries);
        let snap = Snapshot::new(0, cat.entries.clone());
        let raw = snapshot::encode(&snap).len();
        let ecat = cat.encode().unwrap().len();
        assert!(ecat < raw, "ecat={ecat} deberia ser < raw={raw}");
    }

    #[test]
    fn cross_reference_finds_volume() {
        let cat = sample();
        let locals = vec![
            std::path::PathBuf::from("E:\\fotos\\a.jpg"),
            std::path::PathBuf::from("C:\\local\\nuevo.bin"),
        ];
        let catalogs = [cat];
        let res = cross_reference(&catalogs, &locals);
        assert_eq!(res[0].1, Some("Backup-USB-01"));
        assert_eq!(res[1].1, None);
    }
}
