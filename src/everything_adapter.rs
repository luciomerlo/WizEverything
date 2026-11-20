//! Adaptador de Everything — única fuente de verdad para metadatos.
//!
//! Regla arquitectónica de la spec: **prohibido recorrer directorios** con
//! `FindFirstFileW`/`WalkDir` para descubrir archivos. Todas las consultas
//! iniciales salen del índice en memoria de Everything con el mask
//! [`everything::REQ_FULL`] (nombre, ruta, tamaño, fechas, atributos, run count).
//!
//! Este módulo traduce `ExtItem` (IPC crudo) a [`Candidate`] (tipado, con
//! `PathBuf`) y ofrece conversiones hacia los motores de análisis.

use std::path::PathBuf;

use crate::everything::{self, ExtItem};

/// Un archivo o carpeta candidato, ya normalizado para los analizadores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub size: u64,
    pub modified: u64,
    pub accessed: u64,
    pub attributes: u32,
    pub run_count: u32,
    pub is_dir: bool,
}

impl From<ExtItem> for Candidate {
    fn from(it: ExtItem) -> Self {
        Candidate {
            path: PathBuf::from(it.full_path()),
            size: it.size.unwrap_or(0),
            modified: it.modified,
            accessed: it.accessed,
            attributes: it.attributes,
            run_count: it.run_count,
            is_dir: it.is_dir,
        }
    }
}

/// Consulta todo el índice y devuelve candidatos (archivos y carpetas).
pub fn query_all(search: &str) -> Result<Vec<Candidate>, String> {
    let mut out = Vec::new();
    everything::for_each_item_ext(search, |it| out.push(Candidate::from(it)), |_, _| {})?;
    Ok(out)
}

/// Consulta solo archivos (excluye carpetas y unidades).
pub fn query_files(search: &str) -> Result<Vec<Candidate>, String> {
    let mut out = Vec::new();
    everything::for_each_item_ext(
        search,
        |it| {
            if !it.is_dir {
                out.push(Candidate::from(it));
            }
        },
        |_, _| {},
    )?;
    Ok(out)
}

/// Convierte candidatos a `dedup::Item` (sin resolver FRN; el pipeline lo hace).
pub fn to_dedup_items(cands: &[Candidate]) -> Vec<crate::dedup::Item> {
    cands
        .iter()
        .filter(|c| !c.is_dir)
        .map(|c| crate::dedup::Item {
            path: c.path.clone(),
            size: c.size,
            file_id: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(name: &str, path: &str, size: Option<u64>, is_dir: bool) -> ExtItem {
        ExtItem {
            is_dir,
            name: name.to_string(),
            path: path.to_string(),
            size,
            modified: 10,
            accessed: 20,
            attributes: 0x200,
            run_count: 3,
        }
    }

    #[test]
    fn converts_ext_item_to_candidate() {
        let c = Candidate::from(ext("a.bin", "C:\\datos", Some(100), false));
        assert_eq!(c.path, PathBuf::from("C:\\datos\\a.bin"));
        assert_eq!(c.size, 100);
        assert_eq!(c.accessed, 20);
        assert!(!c.is_dir);
    }

    #[test]
    fn unknown_size_becomes_zero() {
        let c = Candidate::from(ext("dir", "C:\\", None, true));
        assert_eq!(c.size, 0);
        assert!(c.is_dir);
    }

    #[test]
    fn dedup_items_skip_directories() {
        let cands = vec![
            Candidate::from(ext("a.bin", "C:\\x", Some(5), false)),
            Candidate::from(ext("sub", "C:\\x", None, true)),
        ];
        let items = to_dedup_items(&cands);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].size, 5);
        assert!(items[0].file_id.is_none());
    }
}
