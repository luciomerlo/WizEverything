# Índice de contenido compartido (WizEverything ↔ SortEverything)

## Ruta única
`%LOCALAPPDATA%\WizeShared\content-index.sqlite3`

Variable de entorno para tests/perfil portable: `WIZE_SHARED_INDEX`.
Nunca usar `%APPDATA%` ni una ruta por app: las dos apps deben apuntar al mismo archivo.

## Quién crea la base
No hay un "dueño permanente". Las dos apps abren el mismo archivo:

1. `open()` crea la carpeta y ejecuta el esquema idempotente (`CREATE ... IF NOT EXISTS`).
2. `claim(conn, app)` pide el **lease** de escritura en una transacción `BEGIN IMMEDIATE`.
3. Si no hay lease, o el latido tiene más de `LEASE_TTL_SECS` (300 s), o el dueño es la misma app,
   el lease se concede. La otra app **lee** pero no escribe.
4. Al terminar de indexar, `release(conn, app)`.
5. `heartbeat(conn, app)` renueva el latido en indexaciones largas.

SQLite con `journal_mode=WAL` y `busy_timeout=5 s` serializa el resto. Si las dos arrancan a la
vez, la creación del esquema no se corrompe: una gana el lease y la otra espera.

## Esquema (versión 1)
```sql
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE docs(
  path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
  kind TEXT NOT NULL, text TEXT NOT NULL, updated INTEGER NOT NULL
);
CREATE INDEX docs_updated ON docs(updated);
CREATE TABLE lease(
  id INTEGER PRIMARY KEY CHECK (id = 1), app TEXT NOT NULL,
  pid INTEGER NOT NULL, heartbeat INTEGER NOT NULL
);
```
`docs` guarda el texto extraído por archivo y su versión exacta (`size`+`mtime`). `kind` es
`text|document|presentation|sheet|pdf|ocr`. El texto se trunca a **48.000 caracteres** por documento.
Correo (`.eml`/`.msg`) no se indexa y no existe `kind=mail`.

## API idéntica en ambos repos (`content_index`)
- `db_path() -> PathBuf`
- `open() -> Result<Connection, String>`
- `claim(&Connection, app) -> Result<bool, String>`
- `heartbeat(&Connection, app) -> Result<(), String>`
- `release(&Connection, app) -> Result<(), String>`
- `get(&Connection, path, size, mtime) -> Option<(kind, text)>`
- `put(&Connection, path, size, mtime, kind, text) -> Result<(), String>` (trunca a 48.000 chars)
- `prune(&Connection, missing) -> Result<usize, String>`
- `stats(&Connection) -> Value`
- `lease_status(&Connection) -> Value`

Constantes: `APP_WIZ = "WizEverything"`, `APP_SORT = "SortEverything"`, `LEASE_TTL_SECS = 300`.

## Extracción y opciones (mismas en las dos apps)
- **Fast** (default): texto/código, PDF con `pdftotext` (primeras 3 páginas), y OOXML/ODF
  (`docx/xlsx/pptx/odt/ods/odp/rtf/epub`) leyendo el ZIP/XML. Sin OCR.
- **Deep**: lo anterior completo y OCR de imágenes/PDF escaneado con Tesseract (`spa+eng`).
- Presupuesto: Fast 2,5 s / 4.000 archivos; Deep 60 s / 50.000; resultado `partial`.
- Formatos antiguos `.doc/.ppt/.xls` no se leen (binario OLE): avisar y pedir conversión.
- Misma semántica de `query`: sin distinguir mayúsculas, fragmento ±60/120 caracteres.

## Regla de convivencia
- El que llega primero y obtiene el lease **indexa**; el otro **consulta** el índice.
- Si el índice está incompleto, la app que consulta puede extraer al vuelo **sin escribir**.
- Al cerrar, liberar el lease. Un lease vencido se puede reclamar sin intervención.
- Nunca borrar `content-index.sqlite3` mientras la otra app esté corriendo.
