//! Módulo 1 (motor) — Pipeline de *exact-match* en 3 etapas.
//!
//! Objetivo: encontrar duplicados exactos minimizando I/O físico, en cuatro
//! filtros progresivos (los primeros no tocan el disco):
//!
//! 1. **Tamaño** (en memoria, viene de Everything/MFT): agrupar por
//!    `size > 0` y descartar grupos con un solo elemento.
//! 2. **FRN** (`FILE_ID_INFO`): si todos los miembros de un grupo comparten
//!    el mismo `file_id`, ya son hardlinks entre sí → descartar el grupo.
//! 3. **Hash parcial BLAKE3** de 3 bloques de 4096 bytes (inicio, centro, fin).
//!    Solo se leen ~12 KiB por archivo.
//! 4. **Hash completo BLAKE3** en streaming, únicamente para los grupos que
//!    sobrevivieron al hash parcial.
//!
//! El resultado son grupos de archivos con contenido byte-a-byte idéntico.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::winphys::FileId;

/// Tamaño de bloque del hash parcial (inicio/centro/fin).
pub const BLOCK: u64 = 4096;
/// Buffer del hash completo (streaming, alineado con `FILE_FLAG_SEQUENTIAL_SCAN`).
pub const STREAM_BUF: usize = 1 << 20; // 1 MiB

/// Un candidato tal como lo entrega el adaptador de Everything.
#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    pub size: u64,
    /// Identidad física, si ya se conoce (opcional: si falta, se consulta).
    pub file_id: Option<FileId>,
}

/// Grupo de archivos con contenido idéntico.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DupGroup {
    pub size: u64,
    pub full_hash: [u8; 32],
    pub paths: Vec<PathBuf>,
}

/// Filtra los grupos que ya están deduplicados por hardlinks (mismo FRN).
fn keep_if_distinct<'a>(items: &'a [Item]) -> Vec<&'a Item> {
    let mut seen: Vec<FileId> = Vec::new();
    for it in items {
        match it.file_id {
            Some(id) => {
                if !seen.contains(&id) {
                    seen.push(id);
                }
            }
            None => {
                // Sin FRN conocido: se trata como "distinto" (se resolverá luego).
                return items.iter().collect();
            }
        }
    }
    if seen.len() <= 1 {
        // Todos comparten identidad física: ya son hardlinks del mismo archivo.
        Vec::new()
    } else {
        items.iter().collect()
    }
}

/// Hash BLAKE3 de 3 bloques (inicio, centro, fin) de un archivo.
///
/// Se etiqueta cada bloque con su desplazamiento para que dos archivos con los
/// mismos bytes reordenados no colisionen por accidente.
pub fn hash_partial_file(path: &Path, size: u64) -> std::io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();

    if size <= 3 * BLOCK {
        // Archivo chico: hash directo de todo el contenido.
        let mut buf = Vec::with_capacity(size as usize);
        f.read_to_end(&mut buf)?;
        hasher.update(&(size).to_le_bytes());
        hasher.update(&buf);
        return Ok(*hasher.finalize().as_bytes());
    }

    let offsets: [u64; 3] = [0, size / 2, size - BLOCK];
    let mut buf = [0u8; BLOCK as usize];
    for &off in &offsets {
        f.seek(SeekFrom::Start(off))?;
        f.read_exact(&mut buf)?;
        hasher.update(&off.to_le_bytes());
        hasher.update(&buf);
    }
    hasher.update(&size.to_le_bytes());
    Ok(*hasher.finalize().as_bytes())
}

/// Hash BLAKE3 completo, en streaming con buffer alineado.
pub fn hash_full_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; STREAM_BUF];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Ejecuta el pipeline completo y devuelve los grupos duplicados confirmados.
///
/// Cualquier error de I/O en un archivo concreto lo excluye del resultado sin
/// abortar el resto (un archivo bloqueado no rompe el análisis global).
pub fn classify(items: &[Item]) -> Vec<DupGroup> {
    // --- Etapa 1: agrupar por tamaño (> 0), solo grupos con >= 2 ---
    let mut by_size: HashMap<u64, Vec<Item>> = HashMap::new();
    for it in items {
        if it.size == 0 {
            continue;
        }
        by_size.entry(it.size).or_default().push(it.clone());
    }

    let mut groups: Vec<DupGroup> = Vec::new();

    for (size, members) in by_size {
        if members.len() < 2 {
            continue;
        }

        // --- Etapa 2: descartar grupos que ya son hardlinks entre sí ---
        let survivors = keep_if_distinct(&members);
        if survivors.len() < 2 {
            continue;
        }

        // --- Etapa 3: hash parcial de 3 bloques ---
        let mut by_partial: HashMap<[u8; 32], Vec<&Item>> = HashMap::new();
        for it in survivors {
            if let Ok(h) = hash_partial_file(&it.path, size) {
                by_partial.entry(h).or_default().push(it);
            }
        }

        for (_, partial_members) in by_partial {
            if partial_members.len() < 2 {
                continue;
            }

            // --- Etapa 4: hash completo para confirmar ---
            let mut by_full: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
            for it in partial_members {
                if let Ok(h) = hash_full_file(&it.path) {
                    by_full.entry(h).or_default().push(it.path.clone());
                }
            }
            for (full_hash, mut paths) in by_full {
                if paths.len() >= 2 {
                    paths.sort();
                    groups.push(DupGroup {
                        size,
                        full_hash,
                        paths,
                    });
                }
            }
        }
    }

    groups.sort_by(|a, b| b.paths.len().cmp(&a.paths.len()));
    groups
}

/// Bytes recuperables si en cada grupo se conserva un solo original.
pub fn recoverable_bytes(groups: &[DupGroup]) -> u64 {
    groups
        .iter()
        .map(|g| g.size.saturating_mul(g.paths.len() as u64 - 1))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "wize_dedup_{}_{}_{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        p
    }

    fn item(path: &Path) -> Item {
        Item {
            path: path.to_path_buf(),
            size: std::fs::metadata(path).unwrap().len(),
            file_id: None,
        }
    }

    #[test]
    fn detects_identical_small_files() {
        let a = write_temp("a", b"mismo contenido exacto");
        let b = write_temp("b", b"mismo contenido exacto");
        let c = write_temp("c", b"OTRO contenido distinto");
        let groups = classify(&[item(&a), item(&b), item(&c)]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths.len(), 2);
        for p in [&a, &b, &c] {
            std::fs::remove_file(p).ok();
        }
    }

    #[test]
    fn same_size_different_content_not_grouped() {
        let a = write_temp("d", b"AAAAAAAAAAAAAAAA");
        let b = write_temp("e", b"BBBBBBBBBBBBBBBB");
        let groups = classify(&[item(&a), item(&b)]);
        assert!(groups.is_empty());
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn identical_large_files_use_partial_then_full() {
        // > 12 KiB para forzar el camino de 3 bloques.
        let content: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let a = write_temp("big_a", &content);
        let b = write_temp("big_b", &content);
        let groups = classify(&[item(&a), item(&b)]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths.len(), 2);
        assert_eq!(hash_partial_file(&a, content.len() as u64).unwrap(),
                   hash_partial_file(&b, content.len() as u64).unwrap());
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn differing_tail_is_caught_by_partial_hash() {
        // Iguales al inicio y centro, distinto el último bloque.
        let mut a_bytes = vec![7u8; 40_000];
        let mut b_bytes = vec![7u8; 40_000];
        *b_bytes.last_mut().unwrap() = 9;
        let a = write_temp("tail_a", &a_bytes);
        let b = write_temp("tail_b", &b_bytes);
        let groups = classify(&[item(&a), item(&b)]);
        assert!(groups.is_empty(), "el hash parcial debe detectar el tail distinto");
        a_bytes.clear();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn recoverable_bytes_counts_copies() {
        let a = write_temp("r_a", b"1234567890");
        let b = write_temp("r_b", b"1234567890");
        let c = write_temp("r_c", b"1234567890");
        let groups = classify(&[item(&a), item(&b), item(&c)]);
        assert_eq!(recoverable_bytes(&groups), 20); // 3 copias -> recupera 2
        for p in [&a, &b, &c] {
            std::fs::remove_file(p).ok();
        }
    }
}
