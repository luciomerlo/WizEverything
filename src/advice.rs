//! "Can I delete this?" — a second opinion before touching anything.
//!
//! It combines what the index already knows (size, dates, location) with cheap
//! Win32 probes (hard links, sparse/cloud, compression) and git tracking, and
//! returns a verdict plus the reasons. It never deletes: the action is to stage
//! the path in the action basket (quarantine, reversible).

use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

use crate::tree::Tree;

pub struct Flags {
    /// Inside a regenerable directory (`node_modules`, `target`, `.venv`, ...).
    pub artifact: Option<&'static str>,
    pub temp: bool,
    pub system: bool,
    pub personal: bool,
    pub recycle: bool,
    pub installed_app: bool,
}

/// Pure path classification so it can be tested without a Tree.
pub fn flags(path: &str) -> Flags {
    let p = format!("\\{}", path.to_lowercase().replace('/', "\\").trim_matches('\\'));
    let mut artifact = None;
    for part in p.split('\\').filter(|s| !s.is_empty()) {
        if let Some(k) = crate::sweeper::classify_dir(part) {
            artifact = Some(k.label());
            break;
        }
    }
    if artifact.is_none() {
        for part in p.split('\\').filter(|s| !s.is_empty()) {
            if part.starts_with("target") || matches!(part, "build" | "dist" | "out" | "obj") {
                artifact = Some("build output");
                break;
            }
        }
    }
    let any = |needles: &[&str]| needles.iter().any(|n| p.contains(n));
    Flags {
        artifact,
        temp: any(&[
            "\\temp\\",
            "\\tmp\\",
            "\\cache\\",
            "\\cache2\\",
            "\\softwaredistribution\\download",
            "\\crashdumps\\",
            "\\inetcache",
            "\\thumbcache",
        ]),
        system: any(&[
            "\\windows\\",
            "\\program files\\",
            "\\program files (x86)\\",
            "\\programdata\\",
            "\\system volume information",
            "\\$recycle.bin",
            "\\recovery\\",
            "\\boot\\",
        ]),
        personal: any(&[
            "\\documents\\",
            "\\pictures\\",
            "\\desktop\\",
            "\\music\\",
            "\\videos\\",
            "\\onedrive",
            "\\dropbox",
        ]),
        recycle: p.contains("\\$recycle.bin"),
        installed_app: any(&[
            "\\appdata\\local\\programs\\",
            "\\appdata\\roaming\\",
            "\\appdata\\local\\packages\\",
        ]),
    }
}

fn git_tracked(path: &Path) -> bool {
    let probe = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    let Some(repo) = crate::github_mode::repo_root(probe) else {
        return false;
    };
    let rel = path.strip_prefix(&repo).unwrap_or(path);
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("ls-files")
        .arg("--")
        .arg(rel)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .output()
    else {
        return false;
    };
    out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

fn size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn verdict(score: i32) -> &'static str {
    if score >= 30 {
        "safe"
    } else if score <= -25 {
        "keep"
    } else {
        "caution"
    }
}

/// Explain whether a node of the index is safe to remove.
pub fn explain(t: &Tree, id: u32, deep: bool) -> Value {
    let n = &t.nodes[id as usize];
    let path = t.full_path(id);
    let f = flags(&path);
    let is_dir = n.is_dir();
    let meta = crate::winphys::probe(Path::new(&path)).ok();
    let links = if is_dir {
        0
    } else {
        crate::winphys::hard_link_count(Path::new(&path)).unwrap_or(0)
    };
    // Deep mode asks git; fast mode skips the process and says so.
    let tracked = deep && git_tracked(Path::new(&path));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age_days = now.saturating_sub(n.modified) / 86_400;

    let mut score = 0i32;
    let mut reasons: Vec<String> = Vec::new();

    if f.system || f.recycle {
        score -= 100;
        reasons.push("Está en una carpeta del sistema (Windows/Program Files/ProgramData/Recycle Bin).".into());
    }
    if f.artifact.is_some() {
        score += 35;
        reasons.push(format!(
            "Está dentro de un artefacto regenerable ({}): se reconstruye con la herramienta del proyecto.",
            f.artifact.unwrap_or("?")
        ));
    }
    if f.temp {
        score += 30;
        reasons.push("Es caché/temporal: la app lo vuelve a crear cuando lo necesita.".into());
    }
    if f.recycle {
        score += 60;
        reasons.push("Ya está en la papelera: vaciarla no pierde nada que no hayas descartado.".into());
    }
    if f.installed_app && f.artifact.is_none() {
        score -= 20;
        reasons.push("Es dato de una aplicación instalada: mejor desinstalar desde Configuración.".into());
    }
    if f.personal && f.artifact.is_none() {
        score -= 10;
        reasons.push("Es contenido personal (Documentos/Fotos/Escritorio/Música/Vídeos): revisá el respaldo antes.".into());
    }
    if tracked {
        score -= 25;
        reasons.push("Está versionado en git: podés recuperarlo desde el repositorio.".into());
    }
    if links > 1 {
        score -= 20;
        reasons.push(format!(
            "Tiene {links} hard links: borrar esta ruta no libera espacio hasta borrar todas las copias."
        ));
    }
    if let Some(m) = &meta {
        if m.is_offline() {
            score -= 10;
            reasons.push("Es un placeholder de la nube (offline): borrarlo no libera casi nada local.".into());
        }
        if m.is_sparse() || m.is_thinner_than_logical() {
            reasons.push(format!(
                "Ocupa menos que su tamaño lógico ({} vs {}): el ahorro real es el físico.",
                size(m.physical_size.unwrap_or(n.alloc)),
                size(n.size)
            ));
        }
        if m.is_reparse_point() {
            reasons.push("Es un junction/symlink: borrarlo no borra el destino.".into());
        }
    }
    if !is_dir && age_days < 7 {
        score -= 15;
        reasons.push(format!("Se modificó hace {age_days} días: puede estar en uso."));
    }
    if n.size == 0 {
        score += 15;
        reasons.push("Está vacío (0 bytes).".into());
    }
    if is_dir && n.files == 0 && n.folders == 0 {
        score += 10;
        reasons.push("Carpeta sin contenido.".into());
    }
    if !is_dir {
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
        if matches!(ext.as_str(), "exe" | "msi" | "msix" | "iso" | "dmg") {
            score += 5;
            reasons.push("Es un instalador/ejecutable: se puede volver a descargar.".into());
        }
    }

    let reclaim = if links > 1 || meta.as_ref().is_some_and(|m| m.is_offline()) {
        0
    } else if is_dir {
        n.alloc.max(n.size)
    } else {
        meta.as_ref()
            .and_then(|m| m.physical_size)
            .unwrap_or(n.alloc)
    };
    if !deep {
        reasons.push("Modo rápido: no se comprobó git ni el historial; usá profundo para esa verificación.".into());
    }
    if reasons.is_empty() {
        reasons.push("Sin señales fuertes a favor ni en contra: revisá el contenido antes de decidir.".into());
    }
    if is_dir {
        reasons.push(format!(
            "Carpeta con {} archivos en {} subcarpetas ({}).",
            n.files,
            n.folders,
            size(n.size)
        ));
    }
    let v = verdict(score);
    let confidence = if deep && meta.is_some() { "alta" } else { "media" };
    let mut actions: Vec<&str> = Vec::new();
    if v != "keep" {
        actions.push("Mover a cuarentena (reversible, con manifiesto)");
    }
    actions.push("Ver en el Explorador");
    if f.personal {
        actions.push("Comprobar respaldo antes de borrar");
    }

    json!({
        "kind": "advice",
        "path": path,
        "name": t.name(id),
        "is_dir": is_dir,
        "bytes": n.size,
        "alloc": n.alloc,
        "files": n.files,
        "folders": n.folders,
        "modified_days": age_days,
        "verdict": v,
        "score": score,
        "confidence": confidence,
        "reclaim_bytes": reclaim,
        "reasons": reasons,
        "tags": {
            "artifact": f.artifact,
            "temp": f.temp,
            "system": f.system,
            "personal": f.personal,
            "recycle": f.recycle,
            "git_tracked": tracked,
            "hard_links": links,
        },
        "actions": actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_artifacts_temp_and_system() {
        let f = flags("C:\\dev\\app\\node_modules\\x\\index.js");
        assert_eq!(f.artifact, Some("node_modules"));
        let f = flags("C:\\Users\\A\\AppData\\Local\\Temp\\x.tmp");
        assert!(f.temp);
        let f = flags("C:\\Windows\\System32\\drivers\\etc\\hosts");
        assert!(f.system);
        let f = flags("C:\\$Recycle.Bin\\S-1-5-21\\$R123.txt");
        assert!(f.recycle);
        let f = flags("C:\\Users\\A\\Documents\\tesis.docx");
        assert!(f.personal);
    }

    #[test]
    fn verdict_thresholds() {
        assert_eq!(verdict(35), "safe");
        assert_eq!(verdict(0), "caution");
        assert_eq!(verdict(-30), "keep");
    }

    #[test]
    fn user_files_are_not_auto_safe() {
        let f = flags("C:\\Users\\A\\Pictures\\familia.jpg");
        assert!(f.personal);
        assert!(f.artifact.is_none());
    }
}
