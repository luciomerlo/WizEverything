//! Addons externos: herramientas que ya resuelven lo que la app no hace sola.
//!
//! La app **no** reimplementa Google Photos ni OCR. Detecta herramientas
//! instaladas y propone el flujo mínimo:
//!
//! - **rclone** (`rclone config` → tipo `google photos`): listar y descargar
//!   originales sin API propia. Sólo lectura para WizEverything.
//! - **gphotos-sync**: descarga la biblioteca a una carpeta local; después
//!   WizEverything indexa esa carpeta y compara originales vs comprimidos.
//! - **Tesseract** y **pdftotext**: OCR y texto de PDFs para buscar dentro.
//!
//! Todo es detección local (`--version`), sin red y sin credenciales.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Una herramienta externa y cómo usarla.
#[derive(Clone, Debug)]
pub struct Addon {
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
    pub version: String,
    pub purpose: &'static str,
    pub setup: Vec<&'static str>,
    pub commands: Vec<&'static str>,
}

fn version_of(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let text = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim().to_string();
    Some(line.chars().take(120).collect())
}

fn addon(id: &'static str, name: &'static str, cmd: &str, version_args: &[&str], purpose: &'static str, setup: Vec<&'static str>, commands: Vec<&'static str>) -> Addon {
    let v = version_of(cmd, version_args);
    Addon { id, name, installed: v.is_some(), version: v.unwrap_or_default(), purpose, setup, commands }
}

/// Detecta todos los addons. No descarga nada.
pub fn detect() -> Vec<Addon> {
    vec![
        addon(
            "rclone",
            "rclone · Google Photos",
            "rclone",
            &["version"],
            "Lista y descarga originales de Google Photos sin credenciales propias de la app.",
            vec![
                "Instalá rclone (winget install Rclone.Rclone).",
                "Ejecutá `rclone config` → New remote → nombre `gphotos` → tipo `google photos`.",
                "Autorizá con tu cuenta; rclone guarda el token en su propia config.",
                "WizEverything sólo lee: no borra ni sube nada.",
            ],
            vec![
                "rclone listremotes",
                "rclone config show gphotos",
                "rclone lsjson gphotos:media/all --max-depth 1",
                "rclone copy gphotos:media/all \"D:\\GooglePhotos\" --progress",
            ],
        ),
        addon(
            "gphotos-sync",
            "gphotos-sync · biblioteca local",
            "gphotos-sync",
            &["--version"],
            "Descarga álbumes/originales a una carpeta; luego la app la indexa como cualquier disco.",
            vec![
                "Instalá con `pipx install gphotos-sync` (o pip).",
                "Ejecutá `gphotos-sync \"D:\\GooglePhotos\"` la primera vez y autorizá.",
                "Repetí el comando para actualizar; la app ve los archivos nuevos al refrescar.",
            ],
            vec![
                "gphotos-sync \"D:\\GooglePhotos\"",
                "gphotos-sync --help",
            ],
        ),
        addon(
            "tesseract",
            "Tesseract · OCR",
            "tesseract",
            &["--version"],
            "Lee texto de imágenes y PDFs escaneados para la búsqueda profunda.",
            vec![
                "Instalá Tesseract (UB Mannheim en Windows) y agregalo al PATH.",
                "Idiomas: instalá `spa` y `eng` (tessdata).",
            ],
            vec!["tesseract --list-langs", "tesseract imagen.png stdout -l spa+eng"],
        ),
        addon(
            "pdftotext",
            "pdftotext · PDF",
            "pdftotext",
            &["-v"],
            "Extrae la capa de texto de PDFs para buscar dentro sin OCR.",
            vec!["Instalá Poppler para Windows y agregá `bin` al PATH."],
            vec!["pdftotext -v", "pdftotext -l 3 archivo.pdf -"],
        ),
    ]
}

/// Carpetas locales donde suelen quedar las descargas de Google Photos.
pub fn local_photo_dirs() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(h) = home {
        roots.push(h.clone());
        roots.push(h.join("Pictures"));
        roots.push(h.join("Downloads"));
    }
    for base in roots {
        for name in ["Google Photos", "GooglePhotos", "gphotos-sync", "Takeout", "Google Takeout"] {
            let p = base.join(name);
            if p.is_dir() {
                out.push((name.to_string(), p));
            }
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

/// Estado para la UI/MCP.
pub fn to_json() -> serde_json::Value {
    let addons = detect();
    let dirs: Vec<serde_json::Value> = local_photo_dirs()
        .into_iter()
        .map(|(n, p)| serde_json::json!({ "name": n, "path": p.to_string_lossy() }))
        .collect();
    serde_json::json!({
        "kind": "addons",
        "addons": addons.iter().map(|a| serde_json::json!({
            "id": a.id,
            "name": a.name,
            "installed": a.installed,
            "version": a.version,
            "purpose": a.purpose,
            "setup": a.setup,
            "commands": a.commands,
            "read_only": true,
        })).collect::<Vec<_>>(),
        "local_photo_dirs": dirs,
        "notes": [
            "WizEverything no pide credenciales de Google: el OAuth lo maneja rclone o gphotos-sync.",
            "Para comparar originales vs comprimidos, descargá los originales con un addon y usá Imágenes parecidas.",
            "La app nunca sube ni borra en Google Photos.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_never_panics_and_has_setup() {
        let a = detect();
        assert_eq!(a.len(), 4);
        assert!(a.iter().all(|x| !x.setup.is_empty() && !x.commands.is_empty()));
    }

    #[test]
    fn local_dirs_are_absolute_when_present() {
        for (_, p) in local_photo_dirs() {
            assert!(p.is_absolute());
        }
    }
}
