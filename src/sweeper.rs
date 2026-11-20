//! Módulo 3 — Sweeper contextual de artefactos de desarrollo y cachés.
//!
//! Detecta carpetas de build/dependencias reconstruibles (`node_modules`,
//! `.venv`, `target`, `__pycache__`, …) y decide si el proyecto que las contiene
//! está abandonado, para proponer una limpieza segura.
//!
//! Toda la decisión es pura (sin I/O): el llamador aporta los bytes estimados y
//! la fecha del archivo fuente más reciente del proyecto hermano.

use std::path::Path;

/// Tipo de artefacto detectado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    NodeModules,
    PythonVenv,
    RustTarget,
    PythonCache,
    Gradle,
    DotnetBuild,
    NextCache,
    Dist,
    Terraform,
}

impl ArtifactKind {
    /// Nombre canónico para mostrar en la UI.
    pub fn label(self) -> &'static str {
        match self {
            ArtifactKind::NodeModules => "node_modules",
            ArtifactKind::PythonVenv => ".venv / venv",
            ArtifactKind::RustTarget => "target",
            ArtifactKind::PythonCache => "__pycache__",
            ArtifactKind::Gradle => ".gradle",
            ArtifactKind::DotnetBuild => "bin/obj (Debug)",
            ArtifactKind::NextCache => ".next",
            ArtifactKind::Dist => "dist",
            ArtifactKind::Terraform => ".terraform",
        }
    }
}

/// Reconhece una carpeta como artefacto reconstruible por su nombre.
pub fn classify_dir(name: &str) -> Option<ArtifactKind> {
    match name.to_ascii_lowercase().as_str() {
        "node_modules" => Some(ArtifactKind::NodeModules),
        ".venv" | "venv" | "env" => Some(ArtifactKind::PythonVenv),
        "target" => Some(ArtifactKind::RustTarget),
        "__pycache__" | "pycache" => Some(ArtifactKind::PythonCache),
        ".gradle" => Some(ArtifactKind::Gradle),
        "obj" | "bin" => Some(ArtifactKind::DotnetBuild),
        ".next" => Some(ArtifactKind::NextCache),
        "dist" | "build" => Some(ArtifactKind::Dist),
        ".terraform" => Some(ArtifactKind::Terraform),
        _ => None,
    }
}

/// Umbrales de abandono por defecto (en días sin tocar un archivo fuente).
pub const DEFAULT_IDLE_DAYS: u64 = 90;

/// Un candidato de limpieza evaluado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepCandidate {
    pub path: std::path::PathBuf,
    pub kind: ArtifactKind,
    /// Días desde la última modificación del código fuente hermano.
    pub idle_days: u64,
    /// Bytes que se recuperarían al borrar el artefacto.
    pub bytes: u64,
    /// `true` si supera el umbral de abandono.
    pub safe_to_delete: bool,
}

/// Calcula los días de inactividad a partir de ahora y la fecha (unix segs) del
/// archivo fuente más reciente. Si no hay fuentes, se considera muy inactivo.
pub fn idle_days(now_unix: u64, source_max_modified_unix: Option<u64>) -> u64 {
    match source_max_modified_unix {
        Some(m) if m <= now_unix => (now_unix - m) / 86_400,
        Some(_) => 0,          // fecha futura: tratar como activo
        None => u64::MAX / 2,  // sin fuentes: proyecto vacío/abandonado
    }
}

/// Evalúa un candidato de limpieza.
pub fn evaluate(
    path: &Path,
    kind: ArtifactKind,
    now_unix: u64,
    source_max_modified_unix: Option<u64>,
    bytes: u64,
    threshold_days: u64,
) -> SweepCandidate {
    let days = idle_days(now_unix, source_max_modified_unix);
    SweepCandidate {
        path: path.to_path_buf(),
        kind,
        idle_days: days,
        bytes,
        safe_to_delete: days > threshold_days,
    }
}

/// Extensiones de código fuente que cuentan como "proyecto vivo".
pub const SOURCE_EXTS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "cs", "cpp", "c", "h", "hpp", "go", "java",
    "kt", "rb", "php", "swift", "scala", "toml", "json", "lock",
];

pub fn is_source_ext(ext: &str) -> bool {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    SOURCE_EXTS.contains(&e.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn recognizes_common_artifacts() {
        assert_eq!(classify_dir("node_modules"), Some(ArtifactKind::NodeModules));
        assert_eq!(classify_dir("NODE_MODULES"), Some(ArtifactKind::NodeModules));
        assert_eq!(classify_dir(".venv"), Some(ArtifactKind::PythonVenv));
        assert_eq!(classify_dir("venv"), Some(ArtifactKind::PythonVenv));
        assert_eq!(classify_dir("target"), Some(ArtifactKind::RustTarget));
        assert_eq!(classify_dir("__pycache__"), Some(ArtifactKind::PythonCache));
        assert_eq!(classify_dir(".gradle"), Some(ArtifactKind::Gradle));
        assert_eq!(classify_dir("obj"), Some(ArtifactKind::DotnetBuild));
        assert_eq!(classify_dir(".next"), Some(ArtifactKind::NextCache));
        assert_eq!(classify_dir(".terraform"), Some(ArtifactKind::Terraform));
        assert_eq!(classify_dir("src"), None);
        assert_eq!(classify_dir("documentos"), None);
    }

    #[test]
    fn idle_days_math() {
        let now = 1_700_000_000u64;
        // 100 días atrás.
        let then = now - 100 * 86_400;
        assert_eq!(idle_days(now, Some(then)), 100);
        // Fuente futura -> 0.
        assert_eq!(idle_days(now, Some(now + 999)), 0);
        // Sin fuentes -> enorme.
        assert!(idle_days(now, None) > DEFAULT_IDLE_DAYS);
    }

    #[test]
    fn evaluate_threshold() {
        let now = 1_700_000_000u64;
        let stale = now - 120 * 86_400;
        let fresh = now - 10 * 86_400;

        let c = evaluate(Path::new("C:\\proj\\node_modules"), ArtifactKind::NodeModules, now, Some(stale), 500_000, DEFAULT_IDLE_DAYS);
        assert!(c.safe_to_delete);
        assert_eq!(c.idle_days, 120);

        let c = evaluate(Path::new("C:\\proj\\target"), ArtifactKind::RustTarget, now, Some(fresh), 900_000, DEFAULT_IDLE_DAYS);
        assert!(!c.safe_to_delete);
    }

    #[test]
    fn source_extensions() {
        assert!(is_source_ext("rs"));
        assert!(is_source_ext(".PY"));
        assert!(is_source_ext("tsx"));
        assert!(!is_source_ext("png"));
        assert!(!is_source_ext("exe"));
    }
}
