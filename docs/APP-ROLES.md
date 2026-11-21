# Reparto de funciones — WizEverything / SortEverything / Census

**Estado:** borrador vivo. Última actualización: 2026-11-21 (revisión de Census).
**Fuente única de este documento:** `C:\Users\Lucio\Downloads\WizEverything\docs\APP-ROLES.md`.
Si otra app necesita corregir algo, que lo pida; no se duplica el documento en tres repos.

Este documento no decide todavía qué funciones se eliminan. Sirve para que las tres apps
tengan límites claros y para detectar solapamientos antes de que se conviertan en deuda.

---

## 1. Resumen en una línea

| App | En una línea | Pregunta que responde |
|---|---|---|
| **WizEverything** | Analizador de uso de disco y contenido | “¿Qué ocupa espacio, qué es, y qué puedo hacer con esto?” |
| **SortEverything** | Organizador con plan y deshacer | “¿Dónde debería estar este archivo y cómo lo muevo sin romper nada?” |
| **Census** | Motor de índice NTFS/MFT | “¿Qué archivos existen en este volumen, ahora, sin recorrerlos?” |

---

## 2. Límites duros por app

### WizEverything

**Hace**

- Vista de uso de disco: árbol, treemap, anillos, tipos, tamaños reales, filtros y comparación visual.
- Analiza contenido y metadatos: duplicados, hardlinks, cloud/sparse, artefactos de desarrollo,
  snapshots, magia/integridad, catálogos offline, procedencia, timeline, velocidad, OCR/documentos,
  contenido dentro de comprimidos (planificado), planificador, informe HTML y comparación de máquinas.
- Muestra el resultado y explica el “por qué”: qué está lleno, qué creció, si conviene borrar,
  si está respaldado.
- Puede ejecutar acciones de limpieza **con confirmación explícita y auditoría**: hardlink,
  compresión WOF LZX, cuarentena reversible. No es un organizador general.
- Es dueño del **esquema y las migraciones** de `content-index.sqlite3`, de los extractores,
  del OCR y de `archive_docs`.
- Consume el índice de Everything o Census; no implementa su propio lector de MFT.

**No hace**

- No es dueño de `C.wcat`, de la MFT, del USN Journal ni del contrato IPC de Census.
- No organiza bibliotecas, no aplica reglas de destino, no mueve/copia archivos en masa.
  Eso es SortEverything.
- No reemplaza a Everything/Census como indexador residente.
- No hidrata Google Drive ni lee placeholders en streaming.
- No indexa correo (`.eml`/`.msg`) por decisión de producto.

### SortEverything

**Hace**

- Descubre archivos con el índice de Everything, el de Wiz/Census o su propio walk de respaldo.
- Construye un **plan** de organización por reglas, contenido, eventos, cobertura y correcciones.
- Muestra el plan completo antes de tocar nada.
- Ejecuta **copy / move / hardlink**, nunca delete; journal y **undo** por corrida.
- Detección de duplicados exactos (BLAKE3) y uso read-only del `hash_cache` de Wiz como evidencia.
- Clasificación opcional con modelo local o Groq; si no hay modelo, funciona por reglas.
- MCP con `status`, `preview_plan`, `apply_plan`, `undo`, `journals`, `explain`.

**No hace**

- No es un analizador de espacio ni un visor de treemap.
- No es dueño del índice compartido; sólo escribe en el fallback con lease si Wiz no corre.
- No define la identidad NTFS ni el stream de cambios.
- No debe escanear/hashear/copiar contenido de Google Drive en streaming.

### Census

**Hace**

- Lee la MFT de NTFS y mantiene el índice en RAM (arrays planos + arena UTF-16).
- Snapshot propio por volumen: `%LOCALAPPDATA%\Census\C.wcat` (`CENSUSWCAT`, checksum FNV-1a).
- Servidor IPC `QUERY2W`, registrado en **dos clases**: `CENSUS_IPC` y
  `EVERYTHING_TASKBAR_NOTIFICATION`.
- **Búsqueda por fragmentos**: cada término se busca como subcadena (sin distinguir mayúsculas
  ni acentos) en el nombre del archivo y en las carpetas que lo contienen. Sin regex ni comodines.
  La única alternativa es lenguaje natural con Groq (`census ask`), que traduce a las herramientas MCP.
- El **parser de IPC sigue aceptando** `path:`, `ext:`, `size:`, `dm:`, `rc:` y `file:`/`folder:`
  porque Wiz/Sort mandan esas consultas. Lo que desapareció es la superficie de operadores en la
  ventana de Census, no el protocolo.
- MCP por stdio, CLI (`index`, `query`, `serve`, `mcp`, `gui`, `drives`, `bench`, `ask`).
- Búsqueda en lenguaje natural con Groq (opcional).
- Ventana vacía estilo Everything: la tabla aparece sólo cuando hay resultados. **No muestra
  árbol de carpetas ni treemap**; eso es de Wiz.
- Residente **sólo por el USN**, para no perder el delta. Escaneo completo de MFT, `--rebuild` y
  releer una unidad son **forzados**; el replay de USN al arrancar sólo se permite si es corto
  (≤ 1 s de journal) y se anuncia en `activity-Census.json`.
- Pausa implementada: `%LOCALAPPDATA%\Census\pause.json`, `census pause --until 22:00 | --for 8h`,
  `census resume`; en pausa reusa el snapshot aunque esté viejo y las consultas siguen respondiendo.
- En progreso: C1 identidad `vol_serial`+`frn`, C2 delta USN, C4 `nightly-state`, C5 nube explícita,
  C6 locks/actividad, robustez de `serve`.
- El shim `Everything64.dll` está **diferido/opcional**: Sort no lo necesita; usa `WM_COPYDATA`
  para Census y la DLL real sólo para Everything 1.4.
- Capacidades: `reply.flags` anuncia sólo los campos que viajan, con
  `PARTIAL_METADATA = 0x8000` cuando falta `run_count`. `serve` aplica
  `ChangeWindowMessageFilterEx` a `WM_COPYDATA`, `WM_USER+401` y `WM_USER+402`: un cliente no
  elevado puede consultar un Census elevado; lo inverso no.

**No hace**

- No extrae texto, no hace OCR, no abre comprimidos, no lee EXIF.
- No analiza duplicados por contenido ni organiza archivos.
- No escribe `content-index.sqlite3`; sólo aporta identidad y cambios.
- No indexa Google Drive/volúmenes virtuales por defecto. Hoy `index_all()` todavía recorre
  G:/H:; C5 lo corrige: nube excluida por defecto, `census drives` la marca
  `virtual · no indexable`, y `--include-cloud` sólo enumera metadata, sin abrir archivos.
- No hace árbol de carpetas ni treemap en su UI.
- No autoindexa MFT ni hace replay largo de USN sin forzarlo.

---

## 3. Matriz de propiedad

| Área | Dueño | Quién consume |
|---|---|---|
| MFT, USN, `C.wcat`, IPC, capacidades | **Census** | Wiz, Sort |
| Índice de archivos de Everything | Everything (externo) | Wiz, Sort |
| Selección Auto/Everything/Census | **Wiz** | — |
| Esquema/migraciones de `content-index.sqlite3` | **Wiz** | Sort (lector; escritor fallback) |
| Extracción de texto, OCR, documentos, comprimidos | **Wiz** | Sort lee el índice; Sort standalone extrae PDF/Office |
| `archive_docs` (comprimidos) | **Wiz** | Sort lee y, en fallback, indexa lo acordado |
| `hash_cache.bin` de Wiz | **Wiz** | Sort (read-only) |
| Duplicados por contenido / hardlinks / cloud-sparse | **Wiz** | Sort usa evidencia |
| Duplicados exactos y organización (copy/move/hardlink) | **Sort** | — |
| Snapshots de inventario `.ecat` / `.wzsnap` | **Wiz** | Sort/otras herramientas |
| Snapshot de índice `.wcat` | **Census** | Wiz vía IPC |
| Informe HTML, comparación de máquinas | **Wiz** | — |
| Pausa/checkpoint de trabajo pesado | Cada app | Census ya; Wiz P0; Sort pendiente |
| Locks de disco/GPU/nube y avisos de conflicto | Contrato compartido | Las tres |
| Prioridad en contención | **Wiz** | Census/Sort ceden |

---

## 4. Ejecución, recursos y prioridad

Reglas vigentes acordadas con las tres apps:

1. **Wiz y Sort sólo trabajan cuando el usuario los fuerza.** No autoarrancan tandas, no
   escanean al iniciar, no se disparan por horario ni por watcher. Las noches se arman a mano.
2. **Census queda residente sólo por el USN.** Su escaneo completo de MFT, `--rebuild` y la
   relectura de una unidad son **forzados**, nunca automáticos. El replay de USN al arrancar sólo
   se permite si el journal es corto (≤ 1 s), se anuncia en `activity-Census.json` y espera si hay otra
   tarea pesada; si el journal es largo, avisa y pide forzarlo. Si el usuario fuerza Wiz y Census
   a la vez, Census pausa en el próximo punto seguro.
3. La exclusión es por **recurso**, no por letra de unidad: `disk:<n>` físico, `cloud:<provider>`,
   `gpu:<n>`, SQLite, snapshots/catálogos y raíces solapadas.
4. Cuando C6 esté implementado, antes de arrancar y durante la tarea se consultarán los
   archivos de actividad de las otras apps y los locks. Si hay conflicto: aviso visible (banner +
   notificación de Windows), con **Cancelar / Esperar / Forzar**. Forzar exige confirmación y queda
   en el log. Mientras C6 no exista, no hay enforcement automático.
5. A mitad de conflicto, la app que cede termina la transacción/archivo/miembro actual, guarda
   checkpoint y libera el lock. Nunca deja SQLite a medias.
6. **Wiz tiene prioridad** si el usuario fuerza varias apps a la vez. Sort/Census pausan en el
   próximo punto seguro y avisan “pausado por WizEverything”.
7. **Pausa persistente** por app: `pause.json` con `paused`, `resume_at`, `reason`.
   El trabajo pesado se corta en límites seguros; las consultas siguen respondiendo con lo indexado.
   Census ya la implementó; Wiz la hace en P0; Sort la implementa en su próxima tanda.
8. **Google Drive/streaming**: metadata-only. No se hidrata, no se hashea, no se hace OCR,
   no se abren comprimidos, no se copia entre volúmenes. Hidratar es una acción explícita aparte.
   Census además excluye las unidades de nube de `index_all` por defecto (C5); `--include-cloud`
   sólo enumera directorios y metadata, nunca abre archivos.

### Actividad y locks (contrato; Sort ya lo implementó, Wiz/Census pendientes)

- **No hay un `activity.json` único.** Cada app escribe sólo su archivo y lee los demás; nunca
  pisa el archivo de otra:
  - `%LOCALAPPDATA%\WizeShared\activity-Wiz.json`
  - `%LOCALAPPDATA%\WizeShared\activity-Sort.json`
  - `%LOCALAPPDATA%\WizeShared\activity-Census.json`
- Cada archivo contiene **una entrada** (objeto) con `pid`, `app`, `task`, `resources`, `roots`,
  `state`, `forced`, `heartbeat`. Si una app necesita dos tareas pesadas, las serializa; no inventa
  un array que las otras tengan que interpretar distinto.
- Escritura atómica (tmp + rename). Un archivo ausente no es error.
- `heartbeat` de más de **120 s** = entrada vieja; no bloquea, pero se puede mostrar como aviso.
- El lock es `locks/disk-<n>.lock` con `LockFileEx` en el primer byte; el sistema lo libera si el
  proceso muere. `disk:<n>` sale de `IOCTL_STORAGE_GET_DEVICE_NUMBER` sobre el volumen montado:
  `C:` y `D:` en el mismo disco dan el mismo número.
- Si no se puede identificar el disco (`disk:?`) o es una nube, **no se inventa un lock**: se avisa
  por solapamiento de raíces y se sigue. Google Drive sigue metadata-only.
- Cuando Wiz está activo, las otras apps calculan `must_yield` y pausan en el próximo punto
  seguro. Sort ya prueba: lock exclusivo real, conflicto `disk:0` con Wiz prioritario, latido
  viejo que no bloquea, y `Downloads2` que **no** cuenta como hijo de `Downloads`.

- `%LOCALAPPDATA%\WizeShared\locks\cloud-<provider>.lock` y `gpu-<n>.lock` quedan disponibles
  para quien pueda identificar el recurso; si no, aplica el aviso por raíces.

### Identidad IPC (C1, acordada; aún no implementada)

- Capacidad nueva: `IDENTITY = 0x0001_0000` en `WM_USER+402`.
- En la respuesta binaria, al final: **16 bytes por ítem**, dos `u64` little-endian
  (`vol_serial`, `frn`). Siempre presente si el cliente anunció la capacidad; no altera el
  `offset` de 8 B por ítem de la respuesta base.
- En JSON/MCP: los mismos valores como **string hexadecimal** (`"vol_serial":"1a2b3c4d"`),
  para no perder bits.
- `frn = 0` = sin identidad → fallback `path + size + mtime`.
- `reply.flags` sólo anuncia los campos que realmente viajan; `PARTIAL_METADATA = 0x8000`
  cuando falta `run_count`. Sin ese bit, Wiz leería `run_count = 0` como “nunca ejecutado”.

---

## 5. Índice de contenido compartido

- Ruta única: `%LOCALAPPDATA%\WizeShared\content-index.sqlite3` (env `WIZE_SHARED_INDEX`).
- SQLite WAL, `busy_timeout` 5 s, lease de 300 s con heartbeat.
- **Wiz escribe si está vivo.** Si no, Sort puede reclamar el lease y escribir. Nunca dos escritores.
- **Census no escribe** el SQLite: aporta identidad (`vol_serial`+`frn`) y, cuando exista, el
  stream de cambios.
- **Sort es lector por defecto.** En modo standalone extrae su propio texto de PDF/Office y no
  depende de Wiz; en modo compartido lee lo que Wiz ya indexó.
- Esquema v2: `vol_serial`, `frn` (64 bits), `source`; `frn=0` = sin identidad y fallback a
  `path+size+mtime`. Migración idempotente por columnas.
- Tope de texto: **48.000 caracteres** por documento.
- Kinds: `text|document|presentation|sheet|pdf|ocr`. Sin `kind=mail`; `.eml`/`.msg` fuera.
- Comprimidos (planificado): tabla separada `archive_docs`, FTS propia, ruta visible
  `archive!member`, hash xxh3, opt-in y con topes fast/deep.

---

## 6. Inventario de funciones de WizEverything

### Núcleo

- Árbol de uso de disco con tamaño lógico, asignado y real; porcentajes, ítems, fechas.
- Treemap y anillos; panel de tipos; filtro por tipo; explorador izquierdo.
- Búsqueda y navegación por teclado; columnas ordenables; export CSV.
- Detección de unidades, etiquetas y Google Drive `(stream)`/`(clone)`; exclusión de unidades.
- Duplicados rápidos (índice) y avanzados (contenido, xxh3 por bloques); carpetas duplicadas.
- Cache de hashes (`hash_cache.bin`) reutilizable.
- Motor Auto/Everything/Census con selector y pin por carga; diálogo de arranque si no hay motor.
- Actualización en vivo cada 30 s; borrados al refrescar/cada 30 min.

### Análisis avanzado (app + MCP)

- Deduplicación y hardlinks (tamaño → file id NTFS → BLAKE3 parcial → completo).
- Auditoría cloud/sparse (tamaño lógico vs asignado).
- Sweeper de artefactos de desarrollo (`node_modules`, `.venv`, `target`, etc.).
- Diff de snapshots (agregado/borrado/crecido/achicado; bytes netos por carpeta).
- Datos fríos + compresión WOF LZX/XPRESS; candidatos y ejecución con confirmación.
- Integridad por magic bytes (truncados/ceros).
- Catálogos portables `.ecat` (Zstd) para volúmenes offline.
- Volúmenes offline: dos retenciones y semihash muestreado.
- Procedencia, timeline, velocidad/ETA, cuarentena reversible, navegador de cuarentena.
- Búsqueda de contenido y OCR (Tesseract/Poppler como addons); documentos
  `docx/odt/rtf/epub/pptx/odp/xlsx/ods`; PDF; imágenes.
- Planificador: “100 GB”, heatmap de edad, WSL/Docker, forecast.
- GitHub mode y GitHub clones (multicopia); Similares (imágenes/audio); A/B de imagen y audio.
- “¿Por qué está lleno?”, “¿Qué creció?”, “¿Puedo borrar esto?”, “¿Está respaldado?”.
- Informe HTML; comparación de dos máquinas; canasta de acciones.
- Fast/Deep en las herramientas que lo admiten; Deep muestra ETA aprendida.

### Experimental / a revisar

- Canasta de acciones, cuarentena y hardlink compiten conceptualmente con Sort.
- OCR/contenido y Sort comparten extracción de texto; el índice compartido debería evitar duplicarla.
- GitHub mode y GitHub clones son dos funciones cercanas.
- Similares (imágenes/audio), A/B de audio, denoise, A/B de imagen: nicho dentro de un analizador de disco.
- Planificador, forecast, comparación de máquinas e informe HTML amplían mucho la superficie.
- 41 herramientas MCP y la matriz Fast/Deep agregan complejidad de mantenimiento.

**No se elimina nada todavía.** Esta lista es para la revisión de producto posterior.

---

## 7. Inventario de funciones de SortEverything

### Descubrimiento

- **Everything 1.4 por su DLL real** (`Everything64.dll`, carga dinámica), verificado en esta
  máquina (`engine: Everything 1.4`).
- **Walk propio con `walkdir`** cuando no hay motor.
- **Census por `WM_COPYDATA`: pendiente**. Hoy `core/census.rs` es un esqueleto de 34 líneas; no
  cuenta como función existente.
- Si el índice responde **vacío** pero la carpeta tiene archivos, recorre la carpeta. Antes podía
  decir “0 archivos” sobre una carpeta llena cuando Everything estaba instalado pero sin indexar.

### Plan

- Destino + motivo + **confianza %** por fila.
- `free_path` evita colisiones; nombres reservados de Windows y rutas largas.
- Saltea archivos con menos de **45 s** (se están escribiendo), directorios protegidos y basura
  conocida.
- El plan completo se muestra **antes** de escribir un byte.

### Aplicar

- **copy / move / hardlink**; nunca borra.
- `_Duplicates` puede ser “skip” (queda en origen para revisar) o carpeta aparte.
- Copy: verifica BLAKE3 y **siempre** registra el hash.
- Move: `rename` en el mismo volumen; entre volúmenes copia + verifica + recién ahí borra el
  original.
- Journal **cada 5 operaciones**, para que un corte de luz no pierda el registro.
- Undo **nunca borra lo que el usuario editó**.

### Identidad por contenido (SHA-256)

- `%APPDATA%\SortEverything\done.json` guarda `sha256 -> acción/destino/tamaño`.
- Si un archivo ya ordenado cambia de ruta, el SHA-256 lo reconoce y no repite la tarea
  (`already_done` en el resultado).
- Deshacer lo olvida.
- SHA-256 implementado sin dependencias, verificado con los vectores del NIST; el journal guarda
  también el `sha256` de cada operación.

### Duplicados

- Detecta **exactos con BLAKE3 propio** y confirma con hash parcial de 64 KB.
- Usa la caché de Wiz como **evidencia read-only**; una caché vieja nunca hace saltear un archivo.
- Funciona **standalone**, sin Wiz.

### Percepción

- Magic bytes y EXIF para detectar extensiones mentirosas y ordenar fotos por fecha real.

### Texto

- PDF con parser propio de zlib dentro del PDF, con reparación de palabras partidas.
- Office: `docx/xlsx/pptx` y `odt/ods/odp` leyendo ZIP/XML a mano, sin crate nuevo.
- Topes: 64 MB de archivo, 8 MB por miembro, 48.000 caracteres.
- La cobertura de facturas funciona igual con PDF que con `.docx`.
- **OCR de escaneados: pendiente** (2 noches); no está implementado.

### IA

- **No carga GGUF en proceso.** Habla HTTP con Ollama, LM Studio o cualquier endpoint
  OpenAI-compatible.
- Groq es opt-in y está **bloqueado** para `medical`, `bank` e `identity`.
- La recomendación de modelo es por RAM (`min_ram_gb`); nunca propone uno que no entre.

### Corrección y UI

- **Caja de corrección en lenguaje natural** (“Recibos de sueldo son nóminas”) que crea una regla
  de usuario con peso 1000 y muestra un diff antes de aplicar.
- Lo mismo por consola: `SortEverything correct "<frase>" [--apply]`.
- `SortEverything <carpeta>` abre la app escaneando esa carpeta.
- Eventos (`2024-11_Casamiento`), insights con `x` permanente, lector del índice compartido por
  defecto.
- `activity-*.json` + locks por disco físico (`LockFileEx`) y nube metadata-only.
- MCP stdio: `status`, `preview_plan`, `apply_plan`, `undo`, `journals`, `explain`.
- Estado en `%APPDATA%\SortEverything` (journal, registry, insights, modelos, `done.json`).
- egui 0.37, abre maximizada, tema/tipografía iguales a Wiz; panel derecho de detalle **sólo con
  fila seleccionada**.
- Hoy **no** tiene: selección múltiple, arrastrar archivos (sólo carpeta por argumento o Browse),
  tabla de reglas editable, botón de pausa (planificado, 1 noche), toggle de OCR, watcher ni tray.

### Residencia y shim

- **No residente.** Sin autoarranque, sin escaneo al iniciar, sin scheduler ni watcher.
- El servidor MCP es **stdio** y vive mientras lo lanza un agente.
- Nada pesado corre sin que el usuario lo fuerce (botón, CLI o MCP).
- La DLL real de Everything se usa **sólo para Everything 1.4**; Census va por `WM_COPYDATA`.
  El placeholder de `Everything64.dll` no bloquea a Sort.

### Tareas profundas de Sort

| Tarea | Noches |
|---|---|
| Cliente `census.rs` (ya tiene las respuestas) | 1 |
| Pausa + checkpoint (`pause.json`, `nightly-state.json`, botón; sin cortar un `move` a mitad de lote) | 1 |
| OCR de Windows (`Windows.Media.Ocr` + `Windows.Data.Pdf`, WinRT por COM a mano; idioma por archivo; en esta máquina hay `es-ES`/`es-MX`, no `en-US`) | 2 |
| Comprimidos en el índice compartido (`archive_docs`, fallback `.zip`/`.tar`/`.tgz`/`.gz`, `archive!member`, xxh3, topes, toggle `archives`) | 1,5–2 |
| `--watch` opcional apagado | 1 |
| Icono embebido como recurso Win32 | 0,5 |

Orden propuesto por Sort: **census → pausa → OCR → comprimidos**, porque census y pausa
desbloquean el trabajo de las tres apps.

---

## 8. Inventario de funciones de Census

- Índice MFT+USN multiunidad; arrays planos; arena UTF-16.
- Snapshot `C.wcat` por volumen, cabecera versionada, checksum, escritura atómica.
- IPC `QUERY2W` registrado en `CENSUS_IPC` y `EVERYTHING_TASKBAR_NOTIFICATION`; capacidad
  `IDENTITY` (C1) pendiente.
- **Búsqueda por fragmentos** en nombre, extensión y carpetas; sin regex/comodines en la ventana.
  El parser IPC conserva `path:`, `ext:`, `size:`, `dm:`, `rc:`, `file:`/`folder:` para Wiz/Sort.
- Ventana vacía estilo Everything; tabla de resultados. Sin árbol/treemap (eso es Wiz).
- CLI, MCP, GUI, `drives`, `bench`, `ask` (Groq).
- Residente sólo por el USN; MFT/`--rebuild`/relectura y replay largo son forzados. Replay corto
  (≤ 1 s) se anuncia en `activity-Census.json`; si hay otra tarea pesada, espera.
- Pausa implementada (`pause.json`, `census pause --until 22:00 | --for 8h`, `census resume`);
  en pausa reusa el snapshot viejo y las consultas siguen. Falta `nightly-state.json` (C4).
- En progreso: C1 identidad `vol_serial`+`frn`, C2 delta USN, C4 checkpoint/docs, C5 nube
  explícita, C6 locks/actividad, robustez de `serve`. Shim diferido/opcional.
- `index_all()` hoy incluye G:/H:; C5 debe excluir nube por defecto, marcar `virtual · no
  indexable` en `census drives` y dejar `--include-cloud` sólo para enumeración metadata-only.
- Capacidades: `PARTIAL_METADATA = 0x8000` cuando falta `run_count`; `ChangeWindowMessageFilterEx`
  para `WM_COPYDATA`, `WM_USER+401` y `WM_USER+402` permite que un cliente no elevado consulte
  un Census elevado (no al revés).

---

## 9. Solapamientos detectados

| Solapamiento | Situación | Decisión provisional |
|---|---|---|
| Duplicados: Wiz vs Sort | Wiz confirma contenido; Sort también detecta exactos con BLAKE3 y organiza | Wiz analiza en profundidad; Sort detecta/organiza standalone; comparten `hash_cache`/índice como evidencia |
| Hardlink/cuarentena: Wiz vs Sort | Wiz tiene canasta; Sort tiene plan/undo | Revisar si la mutación general se muda a Sort |
| Extracción de texto: Wiz vs Sort | Ambos leen PDF/documentos | Modo compartido: Wiz extrae, Sort lee. Standalone: Sort extrae PDF/Office por su cuenta |
| Snapshots: `.ecat` Wiz vs `.wcat` Census | Formatos y propósitos distintos | No unificar; distinto nivel (inventario vs MFT) |
| Delta: snapshot diff Wiz vs USN Census | Wiz diffa inventarios; Census detecta cambios | Complementarios; no duplicar USN en Wiz |
| Google Drive | Wiz lo detecta; Sort podría querer ordenarlo; Census hoy recorre G:/H: | Wiz/Sort metadata-only; C5 excluye nube de `index_all` por defecto y `--include-cloud` sólo enumera metadata |
| Informes/HTML | Wiz exporta; Sort muestra planes | Wiz reporta análisis; Sort no reporta espacio |

---

## 10. Decisiones cerradas y preguntas abiertas

### Cerradas

- **Census residente sólo por el USN**; MFT/`--rebuild`/relectura y replay largo son forzados.
- **Búsqueda de Census**: fragmentos en nombre/extensión/carpetas; sin regex/comodines en la UI.
  El IPC sigue aceptando operadores porque Wiz/Sort los mandan.
- **Identidad**: capacidad `IDENTITY`, 16 B por ítem en binario, hex en JSON/MCP, `frn=0` fallback.
- **Shim `Everything64.dll`**: diferido/opcional; Sort no lo necesita.
- **Nube**: C5, excluida por defecto; `--include-cloud` metadata-only.
- **Pausa**: Census ya la tiene; Wiz la implementa en P0; falta `nightly-state` de Census (C4).
- **Locks/actividad**: Sort ya implementó `activity-*.json` y locks `disk:<n>`;
  Wiz y Census están pendientes. No hay `activity.json` único: cada app escribe el suyo.
- **Sort standalone** extrae PDF/Office por su cuenta; el modo compartido es una optimización,
  no una dependencia de Wiz.

### Abiertas

1. ¿La mutación de Wiz (canasta/cuarentena/hardlink/WOF) se queda en Wiz o se mueve a Sort?
2. ¿La búsqueda dentro de comprimidos es de Wiz, de Sort o del índice compartido?
3. ¿Qué funciones de Wiz son “núcleo” y cuáles “experimentales” para la limpieza de alcance?
4. ¿Se mantienen los 41 tools MCP o se agrupan en menos operaciones?

---

## 11. Cómo se actualiza

- Cualquier decisión de reparto se registra acá en la misma fecha.
- Si una app necesita un campo/contrato nuevo, lo pide al dueño; no lo implementa por su cuenta.
- Las discusiones de nocturnas van en `docs/NIGHTLY-PLAN.md` de Census; este documento define
  **quién hace qué**, no el calendario.
- Cuando el usuario decida recortar funciones, se agrega una sección “Decisiones de alcance”
  con fecha y se actualiza la matriz; no se borra el historial.
