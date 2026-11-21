//! Detección e instalación de los motores de índice.
//!
//! WizEverything no incluye ni redistribuye Everything ni Census. Si al
//! arrancar no hay ninguno, la app ofrece instalarlos o apuntar a un Census
//! existente, sin descargar nada por su cuenta.

use std::path::{Path, PathBuf};

use crate::i18n::IndexEngine;

#[derive(Clone, Debug, Default)]
pub struct Detection {
    pub everything_running: bool,
    pub everything_installed: Option<PathBuf>,
    pub census_running: bool,
    pub census_installed: Option<PathBuf>,
    pub winget: bool,
}

fn exists_file(p: &Path) -> Option<PathBuf> {
    p.is_file().then(|| p.to_path_buf())
}

fn which(exe: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("where.exe").arg(exe).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(PathBuf::from)
}

/// Rutas habituales de Everything (instalador oficial y winget).
pub fn everything_candidates() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from(r"C:\Program Files\Everything\Everything.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Everything\Everything.exe"),
    ];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        v.push(PathBuf::from(local).join(r"Programs\Everything\Everything.exe"));
    }
    v
}

/// Rutas habituales de Census, más una ruta elegida por el usuario.
pub fn census_candidates(extra: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if !extra.trim().is_empty() {
        v.push(PathBuf::from(extra.trim().trim_matches('"')));
    }
    if let Ok(p) = std::env::var("WIZE_CENSUS_PATH") {
        v.push(PathBuf::from(p));
    }
    v.push(PathBuf::from("census.exe"));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        v.push(PathBuf::from(local).join(r"Census\census.exe"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        v.push(PathBuf::from(pf).join(r"Census\census.exe"));
    }
    v
}

pub fn detect(census_extra: &str) -> Detection {
    let ev = everything_candidates().into_iter().find_map(|p| exists_file(&p)).or_else(|| which("Everything.exe"));
    let ce = census_candidates(census_extra).into_iter().find_map(|p| exists_file(&p)).or_else(|| which("census.exe"));
    Detection {
        everything_running: crate::everything::status_of(IndexEngine::Everything)
            == crate::everything::Status::Ready,
        everything_installed: ev,
        census_running: crate::everything::status_of(IndexEngine::Census) == crate::everything::Status::Ready,
        census_installed: ce,
        winget: which("winget.exe").is_some(),
    }
}

/// Lanza el instalador oficial de Everything con winget. La app no descarga
/// nada por sí misma: winget muestra el progreso y pide los permisos.
pub fn install_everything() -> Result<(), String> {
    std::process::Command::new("winget.exe")
        .args([
            "install",
            "--id",
            "voidtools.Everything",
            "-e",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("No se pudo ejecutar winget: {e}"))
}

pub fn open_everything_download() {
    let _ = std::process::Command::new("cmd.exe")
        .args(["/C", "start", "", "https://www.voidtools.com/downloads/"])
        .spawn();
}

pub fn start_everything(path: &Path) -> Result<(), String> {
    std::process::Command::new(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("No se pudo iniciar Everything: {e}"))
}

fn system_drive() -> String {
    std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())
}

/// Inicia `census serve` para el volumen del sistema. Census necesita elevación
/// para leer la MFT; si Windows la pide, el usuario la concede en el diálogo.
pub fn start_census(path: &Path) -> Result<(), String> {
    // `census serve` necesita leer \.\C:, así que se pide elevación con el
    // diálogo estándar de Windows. Si el usuario la rechaza, no se fuerza nada.
    #[cfg(windows)]
    {
        use std::ptr::{null, null_mut};
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }
        let file = wide(&path.to_string_lossy());
        let params = wide(&format!("serve --volume {}", system_drive()));
        let verb = wide("runas");
        let r = unsafe {
            ShellExecuteW(
                null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                null(),
                SW_SHOWNORMAL,
            )
        };
        // ShellExecute devuelve un HINSTANCE <= 32 cuando falla.
        if r as isize <= 32 {
            return Err(format!(
                "No se pudo iniciar Census con permisos de administrador (código {}).",
                r as isize
            ));
        }
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(path)
            .args(["serve", "--volume", &system_drive()])
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("No se pudo iniciar Census: {e}"))
    }
}

pub fn open_folder(path: &Path) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

/// Estado legible para la UI.
pub fn status_text(running: bool, installed: Option<&Path>) -> String {
    if running {
        crate::i18n::tr("en ejecución", "running").to_string()
    } else if let Some(p) = installed {
        format!("{} {}", crate::i18n::tr("instalado en", "installed at"), p.display())
    } else {
        crate::i18n::tr("no detectado", "not detected").to_string()
    }
}
