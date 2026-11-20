//! Procedencia: de dónde salió un archivo y qué programa lo instaló.
//!
//! - **Zona de Internet** (ADS `Zone.Identifier`): si vino de la web, de qué
//!   URL, y si Windows lo marcó como descargado.
//! - **Aplicación instalada**: mapa de `InstallLocation` del Registro
//!   (Uninstall, 64/32 bits y HKCU), steam libraries y paquetes de la Store.
//! - **Veces abierto** (`run_count`) y última fecha de acceso: se consultan a
//!   Everything sólo en modo profundo.
//!
//! Todo es de sólo lectura y no abre el contenido de los archivos.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_EXPAND_SZ,
    REG_SZ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
};

use crate::tree::Tree;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_utf16(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Información de la ADS `Zone.Identifier` de un archivo descargado.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Zone {
    pub zone_id: u32,
    /// 3 = Internet, 2 = intranet de confianza, 1 = intranet local, 0 = equipo local.
    pub zone_name: String,
    pub host_url: String,
    pub referrer_url: String,
    pub package: String,
}

impl Zone {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "zone_id": self.zone_id,
            "zone": self.zone_name,
            "host_url": self.host_url,
            "referrer_url": self.referrer_url,
            "package": self.package,
        })
    }
}

/// Lee `Zone.Identifier` sin abrir el contenido del archivo.
pub fn zone(path: &Path) -> Option<Zone> {
    let ads = format!("{}:Zone.Identifier", path.display());
    let text = std::fs::read_to_string(ads).ok()?;
    let mut z = Zone::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "ZoneId" => z.zone_id = v.trim().parse().unwrap_or(0),
            "HostUrl" => z.host_url = v.trim().to_string(),
            "ReferrerUrl" => z.referrer_url = v.trim().to_string(),
            "LastWriterPackageFamilyName" => z.package = v.trim().to_string(),
            _ => {}
        }
    }
    z.zone_name = match z.zone_id {
        0 => "equipo local",
        1 => "intranet local",
        2 => "sitios de confianza",
        3 => "Internet",
        4 => "sitios restringidos",
        _ => "desconocida",
    }
    .into();
    Some(z)
}

/// Una aplicación instalada y su carpeta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub name: String,
    pub location: PathBuf,
    pub version: String,
    pub publisher: String,
    pub hive: &'static str,
}

fn reg_string(hkey: HKEY, name: &str) -> Option<String> {
    let w = wide(name);
    let mut ty = 0u32;
    let mut len = 0u32;
    let rc = unsafe { RegQueryValueExW(hkey, w.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut len) };
    if rc != ERROR_SUCCESS || len == 0 || len > 4 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    let rc = unsafe { RegQueryValueExW(hkey, w.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut len) };
    if rc != ERROR_SUCCESS {
        return None;
    }
    if ty == REG_SZ || ty == REG_EXPAND_SZ {
        let u16s: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        let s = String::from_utf16_lossy(&u16s).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    } else {
        None
    }
}

fn clean_location(raw: &str) -> Option<PathBuf> {
    let mut s = raw.trim().trim_matches('"').to_string();
    if let Some((p, _)) = s.rsplit_once(',') {
        // DisplayIcon suele ser "C:\app\app.exe,0".
        if p.to_ascii_lowercase().ends_with(".exe") || p.to_ascii_lowercase().ends_with(".ico") {
            s = p.trim().to_string();
        }
    }
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(&s);
    if p.is_file() { p.parent().map(Path::to_path_buf) } else { Some(p) }
}

fn scan_hive(root: HKEY, sub: &str, view: u32, hive: &'static str, out: &mut Vec<Install>) {
    let wsub = wide(sub);
    let mut h: HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(root, wsub.as_ptr(), 0, KEY_READ | view, &mut h) };
    if rc != ERROR_SUCCESS {
        return;
    }
    for i in 0..20_000u32 {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(h, i, name.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        };
        if rc != ERROR_SUCCESS {
            break;
        }
        let key = from_utf16(&name[..len as usize]);
        let wk = wide(&key);
        let mut kh: HKEY = std::ptr::null_mut();
        if unsafe { RegOpenKeyExW(h, wk.as_ptr(), 0, KEY_READ | view, &mut kh) } != ERROR_SUCCESS {
            continue;
        }
        if let Some(name) = reg_string(kh, "DisplayName") {
            let loc = reg_string(kh, "InstallLocation")
                .or_else(|| reg_string(kh, "DisplayIcon"))
                .and_then(|s| clean_location(&s));
            if let Some(location) = loc {
                out.push(Install {
                    name,
                    location,
                    version: reg_string(kh, "DisplayVersion").unwrap_or_default(),
                    publisher: reg_string(kh, "Publisher").unwrap_or_default(),
                    hive,
                });
            }
        }
        unsafe { RegCloseKey(kh) };
    }
    unsafe { RegCloseKey(h) };
}

/// Aplicaciones instaladas, escaneadas una vez por proceso.
pub fn installs() -> &'static [Install] {
    static INSTALLS: OnceLock<Vec<Install>> = OnceLock::new();
    INSTALLS.get_or_init(|| {
        let mut out = Vec::new();
        for (view, tag) in [(KEY_WOW64_64KEY, "HKLM64"), (KEY_WOW64_32KEY, "HKLM32")] {
            scan_hive(HKEY_LOCAL_MACHINE, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall", view, tag, &mut out);
        }
        scan_hive(HKEY_CURRENT_USER, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall", KEY_WOW64_64KEY, "HKCU", &mut out);
        out
    })
}

fn steam_path() -> Option<PathBuf> {
    let wsub = wide("Software\\Valve\\Steam");
    let mut h: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wsub.as_ptr(), 0, KEY_READ, &mut h) } != ERROR_SUCCESS {
        return None;
    }
    let p = reg_string(h, "SteamPath").map(PathBuf::from);
    unsafe { RegCloseKey(h) };
    p.filter(|p| p.exists())
}

/// Bibliotecas de Steam declaradas en `libraryfolders.vdf`.
pub fn steam_libraries() -> Vec<PathBuf> {
    let Some(steam) = steam_path() else { return Vec::new() };
    let mut libs = vec![steam.clone()];
    let vdf = steam.join("steamapps").join("libraryfolders.vdf");
    if let Ok(text) = std::fs::read_to_string(vdf) {
        for line in text.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("\"path\"") {
                let v = rest.trim().trim_matches('"').replace("\\\\", "\\");
                if !v.is_empty() {
                    libs.push(PathBuf::from(v));
                }
            }
        }
    }
    libs.sort();
    libs.dedup();
    libs
}

/// Veces que Everything registró la apertura del archivo.
pub fn run_count(path: &str) -> Option<u32> {
    let mut found = None;
    let search = format!("\"{}\"", path.replace('"', ""));
    let _ = crate::everything::for_each_item_ext(
        &search,
        |it| {
            if it.full_path().eq_ignore_ascii_case(path) {
                found = Some(it.run_count);
            }
        },
        |_, _| {},
    );
    found
}

fn under(path: &Path, root: &Path) -> bool {
    let p = path.to_string_lossy().to_lowercase().replace('/', "\\");
    let r = root.to_string_lossy().to_lowercase().replace('/', "\\").trim_end_matches('\\').to_string();
    !r.is_empty() && (p == r || p.starts_with(&format!("{r}\\")))
}

/// Explica la procedencia de un nodo. `deep` agrega `run_count` (consulta Everything).
pub fn explain(t: &Tree, id: u32, deep: bool) -> serde_json::Value {
    let path = t.full_path(id);
    let p = Path::new(&path);
    let node = &t.nodes[id as usize];
    let z = zone(p);
    let install = installs()
        .iter()
        .filter(|i| under(p, &i.location))
        .max_by_key(|i| i.location.as_os_str().len())
        .map(|i| {
            serde_json::json!({
                "name": i.name,
                "version": i.version,
                "publisher": i.publisher,
                "location": i.location.to_string_lossy(),
                "hive": i.hive,
            })
        });
    let steam = steam_libraries().into_iter().find(|l| under(p, l)).map(|l| l.to_string_lossy().to_string());
    let mut sources: Vec<String> = Vec::new();
    if let Some(z) = &z {
        if !z.host_url.is_empty() {
            sources.push("descargado de Internet".into());
        } else if z.zone_id != 0 {
            sources.push(format!("zona {}", z.zone_name));
        }
    }
    if let Some(i) = &install {
        sources.push(format!("instalado por {}", i["name"].as_str().unwrap_or("")));
    }
    if steam.is_some() {
        sources.push("biblioteca de Steam".into());
    }
    if let Some(z) = &z
        && !z.package.is_empty()
    {
        sources.push(format!("paquete {}", z.package));
    }
    let runs = if deep { run_count(&path) } else { None };
    serde_json::json!({
        "kind": "provenance",
        "path": path,
        "type": if node.is_dir() { "folder" } else { "file" },
        "size": node.size,
        "modified": node.modified,
        "zone": z.map(|z| z.to_json()),
        "install": install,
        "steam_library": steam,
        "run_count": runs,
        "sources": sources,
        "notes": [
            "La zona de Internet sale de la ADS Zone.Identifier; si el archivo se copió, puede haberse perdido.",
            "La aplicación instalada se deduce por la carpeta de instalación del Registro, no por el nombre.",
            "run_count sólo en modo profundo: consulta el índice de Everything."
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_falls_back_to_none_for_normal_files() {
        let p = std::env::temp_dir().join(format!("wzprov-{}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        assert!(zone(&p).is_none() || zone(&p).unwrap().zone_id == 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn prefix_matching_respects_folder_boundaries() {
        assert!(under(Path::new("C:\\App\\a.exe"), Path::new("C:\\App")));
        assert!(!under(Path::new("C:\\Application\\a.exe"), Path::new("C:\\App")));
    }

    #[test]
    fn install_scan_does_not_panic() {
        let _ = installs().len();
        let _ = steam_libraries();
    }
}
