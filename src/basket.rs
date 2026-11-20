//! Real backend for the action basket: the only place that can actually touch
//! disk. Every operation is reversible:
//!   - HardlinkMerge: content is verified identical by `hardlink::execute`.
//!   - Quarantine: files are moved to `%LOCALAPPDATA%\WizEverything\quarantine\<stamp>`
//!     with a JSONL manifest so they can be put back.
//!   - Compress: WOF LZX external backing (reversible with `wof::decompress`).
//!
//! Audit: one JSONL line per operation in `%LOCALAPPDATA%\WizEverything\audit.jsonl`,
//! written by the executor before each operation runs.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::executor::{Backend, Op};

fn app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WizEverything")
}

fn stamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("q_{ms}")
}

/// Windows-illegal characters become `_` so a full path can be a folder name.
fn safe_part(part: &str) -> String {
    part.chars()
        .map(|c| match c {
            ':' | '<' | '>' | '"' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect()
}

pub struct RealBackend {
    pub dir: PathBuf,
    pub manifest: Vec<serde_json::Value>,
    pub log: PathBuf,
}

impl RealBackend {
    pub fn new() -> io::Result<Self> {
        let mut be = Self::with_dir(app_data().join("quarantine").join(stamp()))?;
        be.log = app_data().join("audit.jsonl");
        Ok(be)
    }

    /// Tests use their own directory so they never write the real audit log.
    pub fn with_dir(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let log = dir.join("audit.jsonl");
        Ok(Self { dir, manifest: Vec::new(), log })
    }

    fn append_log(&self, line: &str) {
        if let Some(parent) = self.log.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.log) {
            let _ = writeln!(f, "{line}");
        }
    }

    fn size_of(path: &Path) -> u64 {
        crate::winphys::physical_size(path)
            .or_else(|_| fs::metadata(path).map(|m| m.len()))
            .unwrap_or(0)
    }

    fn quarantine(&mut self, path: &Path) -> Result<u64, String> {
        if !path.exists() {
            return Err(format!("no existe: {}", path.display()));
        }
        let size = Self::size_of(path);
        let mut dest = self.dir.clone();
        for part in path.to_string_lossy().split(['\\', '/']).filter(|p| !p.is_empty()) {
            dest.push(safe_part(part));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if fs::rename(path, &dest).is_err() {
            fs::copy(path, &dest).map_err(|e| e.to_string())?;
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        let entry = json!({
            "original": path.to_string_lossy(),
            "quarantined": dest.to_string_lossy(),
            "bytes": size,
        });
        self.manifest.push(entry.clone());
        let line = entry.to_string();
        let mpath = self.dir.join("manifest.jsonl");
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&mpath) {
            let _ = writeln!(f, "{line}");
        }
        self.append_log(&line);
        Ok(size)
    }
}

impl Backend for RealBackend {
    fn estimate(&self, op: &Op) -> Result<u64, String> {
        match op {
            Op::HardlinkMerge { duplicate, .. } | Op::Quarantine { path: duplicate } => {
                if !duplicate.exists() {
                    return Err(format!("no existe: {}", duplicate.display()));
                }
                Ok(Self::size_of(duplicate))
            }
            Op::Compress { path } => {
                let n = fs::metadata(path).map_err(|e| e.to_string())?.len();
                // WOF LZX ratio varies; be conservative in the preview.
                Ok(n / 2)
            }
            _ => Err("operación no permitida en la canasta".into()),
        }
    }

    fn execute(&mut self, op: &Op) -> Result<u64, String> {
        match op {
            Op::HardlinkMerge { master, duplicate } => {
                let freed = Self::size_of(duplicate);
                crate::hardlink::execute(master, duplicate).map_err(|e| e.to_string())?;
                self.append_log(
                    &json!({
                        "op": "hardlink_merge",
                        "master": master.to_string_lossy(),
                        "duplicate": duplicate.to_string_lossy(),
                        "bytes": freed,
                    })
                    .to_string(),
                );
                Ok(freed)
            }
            Op::Quarantine { path } => self.quarantine(path),
            Op::Compress { path } => {
                crate::wof::compress(path, crate::wof::Algorithm::Lzx).map_err(|e| e.to_string())?;
                self.append_log(
                    &json!({ "op": "compress_lzx", "path": path.to_string_lossy() }).to_string(),
                );
                Ok(0)
            }
            _ => Err("operación no permitida en la canasta".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutionPlan, OpStatus};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wize-basket-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn quarantine_moves_and_writes_manifest() {
        let base = tmp("q");
        let src = base.join("src").join("file.txt");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, b"hello").unwrap();
        let mut be = RealBackend::with_dir(base.join("quarantine")).unwrap();
        let freed = be.execute(&Op::Quarantine { path: src.clone() }).unwrap();
        assert_eq!(freed, 5);
        assert!(!src.exists(), "source should be gone");
        assert!(be.dir.join("manifest.jsonl").exists());
        assert_eq!(be.manifest.len(), 1);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn hardlink_merge_keeps_both_paths() {
        let base = tmp("h");
        let a = base.join("a.bin");
        let b = base.join("b.bin");
        fs::write(&a, b"same content").unwrap();
        fs::write(&b, b"same content").unwrap();
        let mut be = RealBackend::with_dir(base.join("quarantine")).unwrap();
        let freed = be
            .execute(&Op::HardlinkMerge { master: a.clone(), duplicate: b.clone() })
            .unwrap();
        assert!(freed > 0);
        assert!(a.exists() && b.exists());
        assert!(crate::hardlink::same_file(&a, &b).unwrap());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn forbidden_ops_are_rejected() {
        let base = tmp("f");
        let mut be = RealBackend::with_dir(base.join("quarantine")).unwrap();
        let err = be.execute(&Op::Delete { path: base.join("x") }).unwrap_err();
        assert!(err.contains("no permitida"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn dry_run_estimates_without_touching() {
        let base = tmp("d");
        let f = base.join("x.bin");
        fs::write(&f, vec![0u8; 4096]).unwrap();
        let mut be = RealBackend::with_dir(base.join("quarantine")).unwrap();
        let rep = crate::executor::run(
            &ExecutionPlan::dry_run(vec![Op::Quarantine { path: f.clone() }]),
            &mut be,
            None,
        );
        assert!(matches!(rep.outcomes[0].status, OpStatus::Planned(_)));
        assert!(f.exists(), "dry-run must not move anything");
        let _ = fs::remove_dir_all(&base);
    }
}
