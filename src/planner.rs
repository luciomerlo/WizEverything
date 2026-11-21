//! Motores de solo lectura: plan de liberación, mapa de calor por antigüedad,
//! discos virtuales de WSL/Docker y pronóstico de disco lleno.
//!
//! Todas las funciones son puras sobre el [`Tree`] ya construido (y sobre los
//! snapshots guardados): nada se borra, mueve ni compacta. Devuelven
//! `serde_json::Value`, así que la UI y el agente consumen lo mismo sin
//! plomería extra.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::snapshot;
use crate::tree::{self, Tree};

/// Candidatos que mira el modo rápido.
const FAST_CANDIDATES: usize = 2_000;
/// Candidatos que mira el modo profundo.
const DEEP_CANDIDATES: usize = 20_000;
const DAY: u64 = 86_400;

/// Segmentos de ruta que marcan un artefacto regenerable.
const SAFE_SEGMENTS: [&str; 10] =
    ["temp", "tmp", "cache", "caches", "node_modules", "target", ".venv", "venv", "__pycache__", "tempfiles"];
/// Archivos de memoria del sistema: nunca se borran.
const KEEP_FILES: [&str; 3] = ["pagefile.sys", "hiberfil.sys", "swapfile.sys"];
const BUCKETS: [&str; 6] = ["<7d", "7-30d", "30-90d", "90-365d", ">365d", "unknown"];

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 * 1000.0 / total as f64).round() / 10.0
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn segments(lower: &str) -> Vec<&str> {
    lower.split('\\').filter(|s| !s.is_empty()).collect()
}

fn leaf(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

/// Riesgo de borrar un archivo: `"keep"`, `"safe"` o `"review"`.
///
/// Gana siempre `keep`: un artefacto dentro de Program Files sigue siendo del
/// sistema. Las rutas se comparan sin distinguir mayúsculas ni el separador.
pub(crate) fn risk_of(path: &str) -> &'static str {
    let p = path.replace('/', "\\").to_lowercase();
    let segs = segments(&p);
    let name = segs.last().copied().unwrap_or("");
    let system = segs
        .iter()
        .any(|s| matches!(*s, "windows" | "system32" | "winsxs" | "program files" | "program files (x86)"));
    if system || KEEP_FILES.contains(&name) {
        return "keep";
    }
    if segs.iter().any(|s| SAFE_SEGMENTS.contains(s)) {
        return "safe";
    }
    "review"
}

fn reason_for(path: &str, risk: &str, age_days: Option<u64>) -> String {
    let name = leaf(&path.replace('/', "\\")).to_lowercase();
    match risk {
        "keep" if KEEP_FILES.contains(&name.as_str()) => {
            "Archivo de memoria del sistema (paginación/hibernación): Windows lo necesita.".to_string()
        }
        "keep" => "Está dentro de una carpeta protegida (Windows, System32, WinSxS o Program Files).".to_string(),
        "safe" => {
            "Temporal, caché o artefacto de build (node_modules/target/.venv/__pycache__): se regenera solo."
                .to_string()
        }
        _ => match age_days {
            Some(d) if d >= 365 => {
                format!("Sin señal de descarte y sin cambios hace {d} días: candidato a archivar o revisar.")
            }
            Some(d) => format!("Sin señal de descarte; último cambio hace {d} días: revisá antes de borrar."),
            None => "Sin señal de descarte y sin fecha de modificación: revisá antes de borrar.".to_string(),
        },
    }
}

fn bucket_index(modified: u64, now: u64) -> usize {
    if modified == 0 {
        return 5;
    }
    let age = now.saturating_sub(modified) / DAY;
    match age {
        d if d < 7 => 0,
        d if d < 30 => 1,
        d if d < 90 => 2,
        d if d <= 365 => 3,
        _ => 4,
    }
}

/// Bucket de antigüedad de una fecha de modificación (unix segs, 0 = sin fecha).
pub(crate) fn bucket_of(modified: u64, now: u64) -> &'static str {
    BUCKETS[bucket_index(modified, now)]
}

/// Plan de liberación: los archivos más grandes bajo `root`, de mayor a menor,
/// hasta cubrir `target_bytes` (o hasta agotar `limit` archivos).
///
/// `percent` es el avance hacia el objetivo (`selected_bytes / target_bytes`);
/// `coverage_pct` es qué parte del árbol cubre el plan (`selected_bytes / root`).
/// `partial` queda en `true` cuando no se llegó al objetivo. Nada se borra:
/// es un dry-run.
pub fn plan_reclaim(t: &Tree, root: u32, target_bytes: u64, limit: usize, deep: bool) -> Value {
    let started = std::time::Instant::now();
    let now = now_unix();
    let cap = if deep { DEEP_CANDIDATES } else { FAST_CANDIDATES };
    let candidates = t.largest_files(root, "", cap);
    let root_bytes = t.nodes[root as usize].size;

    let mut items: Vec<Value> = Vec::new();
    let mut selected = 0u64;
    let (mut safe_bytes, mut review_bytes, mut keep_bytes) = (0u64, 0u64, 0u64);
    let mut reached = target_bytes == 0;
    for &id in &candidates {
        if reached || items.len() >= limit {
            break;
        }
        let n = &t.nodes[id as usize];
        if n.size == 0 {
            continue;
        }
        let path = t.full_path(id);
        let risk = risk_of(&path);
        let age_days = (n.modified > 0).then(|| now.saturating_sub(n.modified) / DAY);
        match risk {
            "safe" => safe_bytes += n.size,
            "keep" => {
                // Un plan de liberación nunca propone tocar el sistema.
                keep_bytes += n.size;
                continue;
            }
            _ => review_bytes += n.size,
        }
        selected += n.size;
        items.push(json!({
            "path": path,
            "bytes": n.size,
            "modified": n.modified,
            "age_days": age_days,
            "risk": risk,
            "reason": reason_for(&path, risk, age_days),
        }));
        if selected >= target_bytes {
            reached = true;
        }
    }

    let percent = if target_bytes == 0 { 100.0 } else { pct(selected, target_bytes) };
    let mut notes: Vec<String> = vec![
        "Los archivos vienen ordenados de mayor a menor: se toman hasta cubrir el objetivo.".to_string(),
        "Riesgo: 'safe' = artefacto regenerable, 'review' = decisión humana, 'keep' = sistema, no tocar.".to_string(),
        "Los archivos 'keep' se cuentan aparte y nunca se incluyen en la lista para liberar.".to_string(),
    ];
    if !reached {
        notes.push(format!(
            "No se alcanzó el objetivo con {} candidatos del modo {}: el plan queda parcial.",
            candidates.len(),
            if deep { "profundo" } else { "rápido" }
        ));
    }

    json!({
        "kind": "reclaim_plan",
        "path": t.full_path(root),
        "mode": if deep { "deep" } else { "fast" },
        "target_bytes": target_bytes,
        "selected_bytes": selected,
        "reached": reached,
        "remaining": target_bytes.saturating_sub(selected),
        "percent": percent,
        "coverage_pct": pct(selected, root_bytes),
        "root_bytes": root_bytes,
        "candidates": candidates.len(),
        "candidate_cap": cap,
        "partial": !reached,
        "safe_bytes": safe_bytes,
        "review_bytes": review_bytes,
        "keep_bytes": keep_bytes,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "items": items,
        "note": "dry-run: nothing is deleted",
        "notes": notes,
    })
}

/// Mapa de calor por antigüedad: bytes y archivos por bucket, más los archivos
/// grandes más viejos (los mejores candidatos a archivar).
pub fn age_heatmap(t: &Tree, root: u32, limit: usize) -> Value {
    let now = now_unix();
    let (mut bytes, mut files) = ([0u64; 6], [0u64; 6]);
    let mut oldest: BinaryHeap<(u64, Reverse<u64>, u32)> = BinaryHeap::new();
    let (mut total, mut count) = (0u64, 0u64);

    for id in t.files_under(root) {
        let n = &t.nodes[id as usize];
        if n.size == 0 {
            continue;
        }
        let b = bucket_index(n.modified, now);
        bytes[b] += n.size;
        files[b] += 1;
        total += n.size;
        count += 1;
        if n.modified > 0 && limit > 0 {
            // La raíz del heap es el más nuevo: así quedan los `limit` más viejos.
            oldest.push((n.modified, Reverse(n.size), id));
            if oldest.len() > limit {
                oldest.pop();
            }
        }
    }

    let mut oldest: Vec<(u64, u64, u32)> = oldest.into_iter().map(|(m, Reverse(s), id)| (m, s, id)).collect();
    oldest.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let oldest: Vec<Value> = oldest
        .into_iter()
        .map(|(m, size, id)| {
            json!({
                "path": t.full_path(id),
                "bytes": size,
                "modified": m,
                "age_days": now.saturating_sub(m) / DAY,
            })
        })
        .collect();

    let buckets: Vec<Value> = BUCKETS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            json!({
                "bucket": label,
                "bytes": bytes[i],
                "files": files[i],
                "pct": pct(bytes[i], total),
            })
        })
        .collect();

    json!({
        "kind": "age_heatmap",
        "path": t.full_path(root),
        "now": now,
        "total_bytes": total,
        "files": count,
        "buckets": buckets,
        "oldest": oldest,
        "notes": [
            "Los buckets usan la fecha de modificación del índice; 'unknown' agrupa los archivos sin fecha (modified = 0).",
            "Un archivo viejo y grande es el mejor candidato a archivar: mirá 'oldest'.",
            "Los bytes son lógicos: no descuentan compresión ni deduplicación.",
        ],
    })
}

/// Tipo de disco virtual según su ruta (minúsculas y con `\\`).
fn vhdx_kind(path_lower: &str) -> Option<&'static str> {
    let vhdx = path_lower.ends_with(".vhdx");
    if vhdx && path_lower.contains("\\appdata\\local\\packages\\") {
        return Some("wsl");
    }
    if vhdx && (path_lower.contains("\\appdata\\local\\docker\\wsl\\") || path_lower.contains("docker-desktop-data")) {
        return Some("docker");
    }
    None
}

fn ends_with_ignore_case(hay: &str, needle: &str) -> bool {
    hay.len() >= needle.len() && hay.is_char_boundary(hay.len() - needle.len())
        && hay[hay.len() - needle.len()..].eq_ignore_ascii_case(needle)
}

/// Discos virtuales de WSL/Docker: candidatos a **compactar**, no a borrar.
///
/// `reclaimable_candidate` es una cota superior (el tamaño actual de los
/// `.vhdx`); compactar recupera el espacio libre que quedó dentro del disco
/// virtual, nunca el archivo entero.
pub fn wsl_docker(t: &Tree, root: u32, limit: usize) -> Value {
    // (bytes, id, kind, es_carpeta)
    let mut found: Vec<(u64, u32, &'static str, bool)> = Vec::new();

    for id in t.files_under(root) {
        let n = &t.nodes[id as usize];
        if n.size == 0 || !ends_with_ignore_case(t.name(id), ".vhdx") {
            continue;
        }
        let path = t.full_path(id);
        if let Some(kind) = vhdx_kind(&path.to_lowercase()) {
            found.push((n.size, id, kind, false));
        }
    }

    // Docker Desktop también guarda imágenes/volúmenes en ProgramData\Docker.
    for &drive in &t.drives {
        let path = format!("{}\\ProgramData\\Docker", t.name(drive));
        if let Some(id) = t.find_path(&path) {
            let n = &t.nodes[id as usize];
            if n.is_dir() && n.size > 0 {
                found.push((n.size, id, "docker", true));
            }
        }
    }

    found.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    let total_found = found.len();
    found.truncate(limit);

    let (mut wsl_bytes, mut docker_bytes) = (0u64, 0u64);
    let mut reclaimable = 0u64;
    let mut has_wsl = false;
    let mut has_docker = false;
    let items: Vec<Value> = found
        .iter()
        .map(|&(size, id, kind, is_dir)| {
            if kind == "wsl" {
                wsl_bytes += size;
                has_wsl = true;
            } else {
                docker_bytes += size;
                has_docker = true;
            }
            if !is_dir {
                reclaimable += size;
            }
            let note = match (kind, is_dir) {
                ("wsl", _) => {
                    "Disco virtual de una distro WSL: borrar archivos dentro no lo achica; se compacta con 'wsl --shutdown' y Optimize-VHD/diskpart."
                }
                ("docker", true) => {
                    "Datos de Docker Desktop (imágenes, contenedores, logs): se limpia con 'docker system prune', no borrando la carpeta."
                }
                _ => {
                    "Disco virtual de Docker Desktop: se recupera espacio con 'docker system prune' y después compactando el .vhdx."
                }
            };
            json!({
                "path": t.full_path(id),
                "bytes": size,
                "kind": kind,
                "is_dir": is_dir,
                "note": note,
            })
        })
        .collect();

    let mut warnings: Vec<String> = Vec::new();
    if total_found == 0 {
        warnings.push("No se encontraron discos virtuales (ext4.vhdx) de WSL ni datos de Docker.".to_string());
    }
    if has_wsl {
        warnings.push("Pará WSL antes de compactar: 'wsl --shutdown'.".to_string());
    }
    if has_docker {
        warnings.push("Cerrá Docker Desktop antes de compactar: el .vhdx queda bloqueado.".to_string());
    }
    warnings.push("No borres el .vhdx: perderías la distro o los datos de Docker.".to_string());

    json!({
        "kind": "wsl_docker",
        "path": t.full_path(root),
        "items": items,
        "found": total_found,
        "wsl_bytes": wsl_bytes,
        "docker_bytes": docker_bytes,
        "reclaimable_candidate": reclaimable,
        "reclaim_mode": "compaction",
        "warnings": warnings,
        "notes": [
            "Estos archivos no se borran: se compactan para devolver el espacio libre que quedó adentro.",
            "'reclaimable_candidate' es el tamaño actual de los discos virtuales: es una cota superior, no una promesa.",
            "En WSL2, 'docker-desktop' y 'docker-desktop-data' viven bajo AppData\\Local\\Docker\\wsl.",
        ],
    })
}

fn read_snapshot(path: &Path) -> Result<snapshot::Snapshot, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("No pude leer {}: {e}", path.display()))?;
    snapshot::decode(&bytes).map_err(|e| format!("Snapshot inválido {}: {e}", path.display()))
}

/// Pronóstico de disco lleno a partir de los snapshots guardados.
///
/// Compara el primero y el último (el mayor lapso posible) y reparte el
/// crecimiento neto entre los días transcurridos. Sin al menos dos snapshots no
/// hay serie temporal que medir, así que devuelve un error explicativo.
pub fn forecast() -> Result<Value, String> {
    let dir = crate::why::snapshots_dir()?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("No pude leer {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("wzsnap")))
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.len() < 2 {
        return Err(format!(
            "Hacen falta al menos 2 snapshots para pronosticar y hay {} en {}. Guardá otro snapshot (¿Qué creció? → Guardar snapshot) y volvé a intentar.",
            files.len(),
            dir.display()
        ));
    }

    let n = files.len();
    let (old, new) = match (read_snapshot(&files[0]), read_snapshot(&files[n - 1])) {
        (Ok(o), Ok(nw)) => (o, nw),
        _ => (read_snapshot(&files[n - 2])?, read_snapshot(&files[n - 1])?),
    };

    let span_days = new.created.saturating_sub(old.created) as f64 / DAY as f64;
    if span_days <= 0.0 {
        return Err("Los snapshots son del mismo instante (o el último es más viejo): no hay lapso para calcular el ritmo de crecimiento.".to_string());
    }

    let old_bytes = old.total_logical();
    let new_bytes = new.total_logical();
    let net_bytes = new_bytes as i64 - old_bytes as i64;
    let per_day = net_bytes as f64 / span_days;

    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let free_bytes = tree::disk_space(&drive).map(|(_, free)| free);
    let days_to_full = free_bytes.filter(|_| per_day > 0.0).map(|free| round1(free as f64 / per_day));

    let confidence = if n >= 7 && span_days >= 7.0 {
        "high"
    } else if n >= 3 && span_days >= 1.0 {
        "medium"
    } else {
        "low"
    };

    let mut notes: Vec<String> = vec![
        "El ritmo es el crecimiento neto (snapshot nuevo − viejo) dividido por los días transcurridos.".to_string(),
        "'days_to_full' usa el espacio libre real de la unidad del sistema; si el disco se achica, no se llena.".to_string(),
    ];
    if net_bytes < 0 {
        notes.push("El índice se achicó en el lapso medido: el pronóstico no aplica.".to_string());
    }
    if confidence == "low" {
        notes.push("Pocos snapshots o lapso corto: tomalo como orden de magnitud, no como fecha.".to_string());
    }

    Ok(json!({
        "kind": "forecast",
        "samples": n,
        "span_days": round1(span_days),
        "old_bytes": old_bytes,
        "new_bytes": new_bytes,
        "net_bytes": net_bytes,
        "per_day": round1(per_day),
        "free_bytes": free_bytes,
        "drive": drive,
        "days_to_full": days_to_full,
        "confidence": confidence,
        "first": { "file": files[0].file_name().map(|f| f.to_string_lossy().to_string()), "created": old.created, "entries": old.entries.len() },
        "last": { "file": files[n - 1].file_name().map(|f| f.to_string_lossy().to_string()), "created": new.created, "entries": new.entries.len() },
        "notes": notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sistema_y_memoria_son_keep() {
        assert_eq!(risk_of(r"C:\Windows\System32\kernel32.dll"), "keep");
        assert_eq!(risk_of(r"C:\Windows\WinSxS\amd64_x\f.dll"), "keep");
        assert_eq!(risk_of(r"C:\Program Files\App\app.exe"), "keep");
        assert_eq!(risk_of(r"C:\Program Files (x86)\Viejo\a.dll"), "keep");
        assert_eq!(risk_of(r"C:\hiberfil.sys"), "keep");
        assert_eq!(risk_of(r"D:\pagefile.sys"), "keep");
        assert_eq!(risk_of(r"D:\swapfile.sys"), "keep");
    }

    #[test]
    fn artefactos_regenerables_son_safe() {
        assert_eq!(risk_of(r"C:\Users\me\AppData\Local\Temp\tmp123\a.tmp"), "safe");
        assert_eq!(risk_of(r"C:\Users\me\AppData\Local\Cache\data.bin"), "safe");
        assert_eq!(risk_of(r"C:\proj\node_modules\pkg\index.js"), "safe");
        assert_eq!(risk_of(r"C:\proj\target\debug\app.exe"), "safe");
        assert_eq!(risk_of(r"C:\proj\.venv\Lib\site.py"), "safe");
        assert_eq!(risk_of(r"C:\proj\src\__pycache__\m.cpython-312.pyc"), "safe");
    }

    #[test]
    fn datos_personales_son_review() {
        assert_eq!(risk_of(r"C:\Users\me\Documents\tesis.docx"), "review");
        assert_eq!(risk_of(r"D:\videos\feria.mp4"), "review");
        assert_eq!(risk_of(r"C:\WindowsApps\no-es-una-carpeta-del-sistema.exe"), "review");
    }

    #[test]
    fn keep_gana_sobre_safe() {
        assert_eq!(risk_of(r"C:\Program Files\App\temp\cache.bin"), "keep");
        assert_eq!(risk_of(r"C:\Windows\Temp\setup.log"), "keep");
    }

    #[test]
    fn buckets_por_antiguedad() {
        let now = 1_700_000_000u64;
        assert_eq!(bucket_of(0, now), "unknown");
        assert_eq!(bucket_of(now - DAY, now), "<7d");
        assert_eq!(bucket_of(now - 6 * DAY, now), "<7d");
        assert_eq!(bucket_of(now - 7 * DAY, now), "7-30d");
        assert_eq!(bucket_of(now - 29 * DAY, now), "7-30d");
        assert_eq!(bucket_of(now - 30 * DAY, now), "30-90d");
        assert_eq!(bucket_of(now - 89 * DAY, now), "30-90d");
        assert_eq!(bucket_of(now - 90 * DAY, now), "90-365d");
        assert_eq!(bucket_of(now - 365 * DAY, now), "90-365d");
        assert_eq!(bucket_of(now - 366 * DAY, now), ">365d");
        // Fecha futura (reloj adelantado) cuenta como reciente.
        assert_eq!(bucket_of(now + 10 * DAY, now), "<7d");
    }

    #[test]
    fn motivo_coherente_con_el_riesgo() {
        for risk in ["safe", "review", "keep"] {
            assert!(!reason_for(r"C:\x\y.bin", risk, Some(400)).is_empty());
        }
        assert!(reason_for(r"C:\pagefile.sys", "keep", None).contains("memoria"));
        assert!(reason_for(r"D:\fotos\a.jpg", "review", Some(400)).contains("400"));
    }
}
