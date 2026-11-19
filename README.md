# WizEverything

Visualizador de espacio en disco estilo **WizTree** que no escanea el disco:
lee el índice que **Everything** (voidtools) ya mantiene en memoria, vía su IPC
(`WM_COPYDATA`, protocolo QUERY2 del SDK; no requiere `Everything64.dll`).

- ~2 M de archivos en 2–6 s, sin permisos de administrador.
- Vista de árbol con % del padre, tamaño, asignado (aprox.), elementos, archivos, carpetas, fecha.
- Panel de tipos de archivo con colores; clic para resaltar en el treemap.
- Treemap *squarified* con sombreado *cushion*, zoom en carpetas, menú contextual.
- Columnas ordenables (clic: ordenar · 2º clic: invertir · 3er clic: quitar orden).
- Bandeja del sistema, instancia única, opción "Iniciar con Windows" (`--background`).

## Requisitos

- Everything 1.4+ en ejecución.
- Rust (toolchain `stable-x86_64-pc-windows-gnu`, fijado en `rust-toolchain.toml`).
- MinGW-w64: `winget install BrechtSanders.WinLibs.POSIX.UCRT`

## Compilar

```powershell
.\build.ps1
```

Genera `target\release\wizeverything.exe`.

## Notas

- "Asignado" es una aproximación (tamaño redondeado al clúster de la unidad);
  Everything no expone el tamaño en disco real (compresión, archivos dispersos, residentes en MFT).
