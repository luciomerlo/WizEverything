//! Ejecución transaccional y segura de operaciones sobre disco.
//!
//! Regla de la spec: **toda operación mutativa es dry-run por defecto** y deja un
//! log de auditoría estructurado (JSONL) antes de tocar el disco.
//!
//! El executor no conoce los detalles de Win32: recibe un [`Backend`] que sabe
//! *estimar* (sin mutar) y *ejecutar* cada [`Op`]. Así los tests usan un mock y la
//! producción inyecta los motores reales (hardlinks, deshidratación, LZX).

use std::io::Write;
use std::path::PathBuf;

/// Operación mutativa sobre el disco.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Fusionar un duplicado a un hardlink del maestro (Módulo 1).
    HardlinkMerge { master: PathBuf, duplicate: PathBuf },
    /// Borrar (idealmente a la papelera) una ruta.
    Delete { path: PathBuf },
    /// Comprimir con WOF LZX (Módulo 5).
    Compress { path: PathBuf },
    /// Deshidratar un placeholder de nube (Módulo 2).
    Dehydrate { path: PathBuf },
}

impl Op {
    /// Descripción legible (para auditoría/UI).
    pub fn describe(&self) -> String {
        match self {
            Op::HardlinkMerge { master, duplicate } => {
                format!("hardlink merge {} -> {}", duplicate.display(), master.display())
            }
            Op::Delete { path } => format!("delete {}", path.display()),
            Op::Compress { path } => format!("compress-lzx {}", path.display()),
            Op::Dehydrate { path } => format!("dehydrate {}", path.display()),
        }
    }

    /// Nombre corto del tipo de operación.
    pub fn kind(&self) -> &'static str {
        match self {
            Op::HardlinkMerge { .. } => "hardlink_merge",
            Op::Delete { .. } => "delete",
            Op::Compress { .. } => "compress_lzx",
            Op::Dehydrate { .. } => "dehydrate",
        }
    }
}

/// Estado del resultado de una operación.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpStatus {
    /// Simulado: se estimó `bytes` recuperables sin tocar el disco.
    Planned(u64),
    /// Ejecutado: se liberaron `bytes`.
    Done(u64),
    /// Falló con un motivo.
    Failed(String),
}

impl OpStatus {
    pub fn bytes(&self) -> u64 {
        match self {
            OpStatus::Planned(b) | OpStatus::Done(b) => *b,
            OpStatus::Failed(_) => 0,
        }
    }
    pub fn is_failure(&self) -> bool {
        matches!(self, OpStatus::Failed(_))
    }
}

/// Backend que sabe estimar y ejecutar operaciones físicas.
pub trait Backend {
    /// Calcula los bytes recuperables sin mutar nada.
    fn estimate(&self, op: &Op) -> Result<u64, String>;
    /// Ejecuta la operación y devuelve los bytes liberados.
    fn execute(&mut self, op: &Op) -> Result<u64, String>;
}

/// Plan a ejecutar (dry-run o real).
#[derive(Clone, Debug)]
pub struct ExecutionPlan {
    pub dry_run: bool,
    pub ops: Vec<Op>,
}

impl ExecutionPlan {
    pub fn dry_run(ops: Vec<Op>) -> Self {
        ExecutionPlan { dry_run: true, ops }
    }
    pub fn live(ops: Vec<Op>) -> Self {
        ExecutionPlan { dry_run: false, ops }
    }
}

/// Resultado por operación.
#[derive(Clone, Debug)]
pub struct OpOutcome {
    pub description: String,
    pub kind: &'static str,
    pub status: OpStatus,
}

/// Reporte final de una corrida.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    pub outcomes: Vec<OpOutcome>,
    /// Bytes efectivamente liberados (solo en modo real).
    pub recovered: u64,
    /// Bytes que se liberarían (solo en modo simulación).
    pub potential: u64,
    pub failures: usize,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.failures == 0
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn audit_line(plan_dry: bool, op: &Op, status: &OpStatus) -> String {
    let (st, bytes) = match status {
        OpStatus::Planned(b) => ("planned", *b),
        OpStatus::Done(b) => ("done", *b),
        OpStatus::Failed(_) => ("failed", 0),
    };
    let reason = match status {
        OpStatus::Failed(r) => format!(",\"reason\":\"{}\"", json_escape(r)),
        _ => String::new(),
    };
    format!(
        "{{\"mode\":\"{}\",\"op\":\"{}\",\"target\":\"{}\",\"status\":\"{}\",\"bytes\":{}{}}}",
        if plan_dry { "dry-run" } else { "execute" },
        op.kind(),
        json_escape(&op.describe()),
        st,
        bytes,
        reason
    )
}

/// Ejecuta (o simula) el plan. Si se pasa `audit`, escribe una línea JSONL por
/// operación *antes* de ejecutarla.
pub fn run(
    plan: &ExecutionPlan,
    backend: &mut dyn Backend,
    mut audit: Option<&mut dyn Write>,
) -> Report {
    let mut report = Report {
        dry_run: plan.dry_run,
        ..Default::default()
    };

    for op in &plan.ops {
        let status = if plan.dry_run {
            match backend.estimate(op) {
                Ok(b) => OpStatus::Planned(b),
                Err(e) => OpStatus::Failed(e),
            }
        } else {
            match backend.execute(op) {
                Ok(b) => OpStatus::Done(b),
                Err(e) => OpStatus::Failed(e),
            }
        };

        // Auditoría ANTES de exponer el resultado (ya quedó registrado el intento).
        if let Some(w) = audit.as_deref_mut() {
            let _ = writeln!(w, "{}", audit_line(plan.dry_run, op, &status));
            let _ = w.flush();
        }

        match &status {
            OpStatus::Planned(b) => report.potential += b,
            OpStatus::Done(b) => report.recovered += b,
            OpStatus::Failed(_) => report.failures += 1,
        }
        report.outcomes.push(OpOutcome {
            description: op.describe(),
            kind: op.kind(),
            status,
        });
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Mock {
        executed: usize,
        fail_on: Option<&'static str>,
    }

    impl Backend for Mock {
        fn estimate(&self, _op: &Op) -> Result<u64, String> {
            Ok(100)
        }
        fn execute(&mut self, op: &Op) -> Result<u64, String> {
            self.executed += 1;
            if self.fail_on == Some(op.kind()) {
                return Err("bloqueado por otro proceso".into());
            }
            Ok(100)
        }
    }

    fn sample_ops() -> Vec<Op> {
        vec![
            Op::HardlinkMerge {
                master: PathBuf::from("C:\\a\\master.bin"),
                duplicate: PathBuf::from("C:\\a\\dup.bin"),
            },
            Op::Compress { path: PathBuf::from("C:\\docs\\viejo.txt") },
        ]
    }

    #[test]
    fn dry_run_never_executes() {
        let mut mock = Mock::default();
        let report = run(&ExecutionPlan::dry_run(sample_ops()), &mut mock, None);
        assert!(report.dry_run);
        assert_eq!(mock.executed, 0, "dry-run no debe tocar el backend");
        assert_eq!(report.potential, 200);
        assert_eq!(report.recovered, 0);
        assert!(report.ok());
    }

    #[test]
    fn live_executes_and_totals() {
        let mut mock = Mock::default();
        let report = run(&ExecutionPlan::live(sample_ops()), &mut mock, None);
        assert_eq!(mock.executed, 2);
        assert_eq!(report.recovered, 200);
        assert_eq!(report.potential, 0);
        assert!(report.ok());
    }

    #[test]
    fn failures_are_counted_and_reported() {
        let mut mock = Mock {
            executed: 0,
            fail_on: Some("delete"),
        };
        let ops = vec![
            Op::Delete { path: PathBuf::from("C:\\x\\y") },
            Op::Delete { path: PathBuf::from("C:\\x\\z") },
        ];
        let report = run(&ExecutionPlan::live(ops), &mut mock, None);
        assert_eq!(report.failures, 2);
        assert!(!report.ok());
        assert!(report.outcomes.iter().all(|o| o.status.is_failure()));
    }

    #[test]
    fn audit_log_is_jsonl_with_escaped_paths() {
        let mut buf: Vec<u8> = Vec::new();
        let mut mock = Mock::default();
        let ops = vec![Op::Delete { path: PathBuf::from("C:\\a b\\c\"d.txt") }];
        let _ = run(&ExecutionPlan::dry_run(ops), &mut mock, Some(&mut buf));
        let line = String::from_utf8(buf).unwrap();
        assert!(line.starts_with('{') && line.trim_end().ends_with('}'));
        assert!(line.contains("\"mode\":\"dry-run\""));
        assert!(line.contains("\"status\":\"planned\""));
        // Las comillas de la ruta quedaron escapadas, no rompen el JSON.
        assert!(line.contains("\\\"d.txt"));
    }
}
