//! Módulo 2 (núcleo) — Parseo de *reparse points* (junctions y symlinks).
//!
//! Todo lo que empieza con `FILE_ATTRIBUTE_REPARSE_POINT` se resuelve leyendo el
//! `REPARSE_DATA_BUFFER` que devuelve `DeviceIoControl(FSCTL_GET_REPARSE_POINT)`.
//! Acá se decodifica ese buffer (puro, testeable) para obtener el destino real,
//! detectar enlaces rotos y saltar bucles al sumar tamaño de directorios.
//!
//! ```text
//! REPARSE_DATA_BUFFER
//!   ReparseTag          u32
//!   ReparseDataLength   u16
//!   Reserved            u16
//!   SubstituteNameOffset u16
//!   SubstituteNameLength u16
//!   PrintNameOffset      u16
//!   PrintNameLength      u16
//!   Flags                u32   (solo SYMLINK)
//!   PathBuffer           WCHAR[]  (offsets relativos al inicio de PathBuffer)
//! ```

pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
pub const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
pub const IO_REPARSE_TAG_CLOUD_1: u32 = 0x9000_101A;
pub const IO_REPARSE_TAG_APPEXECLINK: u32 = 0x8000_001B;

/// Tipo de reparse point reconocido.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReparseKind {
    MountPoint,
    Symlink,
    Cloud,
    Cloud1,
    AppExecLink,
    Other(u32),
}

impl ReparseKind {
    pub fn from_tag(tag: u32) -> Self {
        match tag {
            IO_REPARSE_TAG_MOUNT_POINT => ReparseKind::MountPoint,
            IO_REPARSE_TAG_SYMLINK => ReparseKind::Symlink,
            IO_REPARSE_TAG_CLOUD => ReparseKind::Cloud,
            IO_REPARSE_TAG_CLOUD_1 => ReparseKind::Cloud1,
            IO_REPARSE_TAG_APPEXECLINK => ReparseKind::AppExecLink,
            other => ReparseKind::Other(other),
        }
    }

    /// `true` si es un enlace que puede generar bucles al recorrer el árbol.
    pub fn is_link(self) -> bool {
        matches!(self, ReparseKind::MountPoint | ReparseKind::Symlink)
    }

    /// `true` si es un placeholder de nube (OneDrive/Drive/Dropbox).
    pub fn is_cloud(self) -> bool {
        matches!(self, ReparseKind::Cloud | ReparseKind::Cloud1)
    }
}

/// Destino resuelto de un reparse point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReparseTarget {
    pub kind: ReparseKind,
    /// Ruta destino (print name si existe; si no, substitute name).
    pub target: String,
    /// `true` si el symlink es relativo.
    pub is_relative: bool,
}

fn u16_at(d: &[u8], off: usize) -> Option<u16> {
    d.get(off..off + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    d.get(off..off + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn utf16_le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Decodifica el destino de un `REPARSE_DATA_BUFFER` de junction o symlink.
///
/// Devuelve `None` si el buffer es demasiado corto o el tag no es un enlace.
pub fn parse_target(data: &[u8]) -> Option<ReparseTarget> {
    let tag = u32_at(data, 0)?;
    let kind = ReparseKind::from_tag(tag);
    if !kind.is_link() {
        return None;
    }
    let reparse_data_len = u16_at(data, 4)? as usize;
    let sub_off = u16_at(data, 8)? as usize;
    let sub_len = u16_at(data, 10)? as usize;
    let print_off = u16_at(data, 12)? as usize;
    let print_len = u16_at(data, 14)? as usize;

    // SYMLINK agrega un campo Flags (u32) antes del PathBuffer.
    let (path_start, is_relative) = if kind == ReparseKind::Symlink {
        let flags = u32_at(data, 16)?;
        (20usize, flags & 0x1 != 0 || flags & 0x8000 != 0)
    } else {
        (16usize, false)
    };

    // El PathBuffer ocupa desde path_start hasta el total declarado.
    let total = 8 + reparse_data_len;
    if total > data.len() || total < path_start {
        return None;
    }
    let path_buf = &data[path_start..total];

    let pick = |off: usize, len: usize| -> Option<String> {
        if len == 0 {
            return None;
        }
        let end = off.checked_add(len)?;
        if end > path_buf.len() {
            return None;
        }
        Some(utf16_le(&path_buf[off..end]))
    };

    let target = pick(print_off, print_len)
        .or_else(|| pick(sub_off, sub_len))
        .unwrap_or_default();

    Some(ReparseTarget {
        kind,
        target,
        is_relative,
    })
}

/// Quita el prefijo `\??\` (NT namespace) que aparece en los substitute names.
pub fn normalize_nt_path(s: &str) -> &str {
    s.strip_prefix("\\??\\").unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construye un REPARSE_DATA_BUFFER sintético.
    fn build(kind: ReparseKind, subst: &str, print: &str, relative: bool) -> Vec<u8> {
        let is_symlink = kind == ReparseKind::Symlink;
        let tag = match kind {
            ReparseKind::MountPoint => IO_REPARSE_TAG_MOUNT_POINT,
            ReparseKind::Symlink => IO_REPARSE_TAG_SYMLINK,
            _ => 0,
        };
        let sub: Vec<u16> = subst.encode_utf16().collect();
        let prn: Vec<u16> = print.encode_utf16().collect();
        let sub_bytes = sub.len() * 2;
        let prn_bytes = prn.len() * 2;

        let header = if is_symlink { 20 } else { 16 };
        let mut path = Vec::new();
        // layout: substitute then print, sin padding (suficiente para el test)
        let sub_off = 0u16;
        let print_off = sub_bytes as u16;
        for u in &sub {
            path.extend_from_slice(&u.to_le_bytes());
        }
        for u in &prn {
            path.extend_from_slice(&u.to_le_bytes());
        }
        let reparse_data_len = (header - 8) + path.len();

        let mut buf = Vec::new();
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&(reparse_data_len as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // Reserved
        buf.extend_from_slice(&sub_off.to_le_bytes());
        buf.extend_from_slice(&(sub_bytes as u16).to_le_bytes());
        buf.extend_from_slice(&print_off.to_le_bytes());
        buf.extend_from_slice(&(prn_bytes as u16).to_le_bytes());
        if is_symlink {
            let flags: u32 = if relative { 0x1 } else { 0 };
            buf.extend_from_slice(&flags.to_le_bytes());
        }
        buf.extend_from_slice(&path);
        buf
    }

    #[test]
    fn parses_junction_target() {
        let buf = build(ReparseKind::MountPoint, "\\??\\D:\\Datos", "D:\\Datos", false);
        let t = parse_target(&buf).unwrap();
        assert_eq!(t.kind, ReparseKind::MountPoint);
        assert_eq!(t.target, "D:\\Datos");
        assert!(!t.is_relative);
        assert!(t.kind.is_link());
    }

    #[test]
    fn parses_symlink_relative() {
        let buf = build(ReparseKind::Symlink, "..\\otro", "..\\otro", true);
        let t = parse_target(&buf).unwrap();
        assert_eq!(t.kind, ReparseKind::Symlink);
        assert_eq!(t.target, "..\\otro");
        assert!(t.is_relative);
    }

    #[test]
    fn rejects_short_or_non_link() {
        assert!(parse_target(&[]).is_none());
        assert!(parse_target(&[0u8; 8]).is_none());
        // Tag de nube no es enlace.
        let mut buf = build(ReparseKind::MountPoint, "x", "x", false);
        buf[0..4].copy_from_slice(&IO_REPARSE_TAG_CLOUD.to_le_bytes());
        assert!(parse_target(&buf).is_none());
    }

    #[test]
    fn kind_classification() {
        assert!(ReparseKind::from_tag(IO_REPARSE_TAG_CLOUD).is_cloud());
        assert!(ReparseKind::from_tag(IO_REPARSE_TAG_CLOUD_1).is_cloud());
        assert!(!ReparseKind::from_tag(IO_REPARSE_TAG_CLOUD).is_link());
        assert!(matches!(ReparseKind::from_tag(0x1234), ReparseKind::Other(0x1234)));
    }

    #[test]
    fn normalizes_nt_prefix() {
        assert_eq!(normalize_nt_path("\\??\\C:\\x"), "C:\\x");
        assert_eq!(normalize_nt_path("C:\\x"), "C:\\x");
    }
}
