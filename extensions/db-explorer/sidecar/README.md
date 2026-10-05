# forge-sql

Database sidecar for the DB Explorer extension. Extensions run in QuickJS without sockets, so the
extension spawns this binary and talks to it over **stdin/stdout, one JSON object per line (UTF-8)**.
Logs go to stderr. The process exits when stdin closes.

Engines: PostgreSQL, MySQL/MariaDB, SQLite (bundled, via `sqlx`), and SQL Server (via `tiberius`).
TLS is rustls only (no OpenSSL), and the binary is self-contained.

## Building and testing

```sh
../scripts/build-sidecar.sh             # release, arm64 + x64 -> ../bin/darwin-{arm64,x64}/forge-sql
../scripts/build-sidecar.sh --host-only # current architecture only

cargo test                              # unit tests + SQLite end-to-end tests
# Server tests run only when their env var holds connect params as JSON:
FORGE_SQL_TEST_PG='{"host":"localhost","port":5432,"user":"postgres","password":"pw","database":"postgres"}' \
FORGE_SQL_TEST_MYSQL='{"host":"localhost","port":3306,"user":"root","password":"pw","database":"forge"}' \
FORGE_SQL_TEST_MSSQL='{"host":"localhost","port":1433,"user":"sa","password":"pw","trustServerCertificate":true}' \
cargo test --test servers
```

## Wire format

```jsonc
→ {"id": 1, "method": "query", "params": {...}}
← {"id": 1, "result": ...}
← {"id": 1, "error": {"message": "human readable", "code": "optional", "index": 0 /* applyChanges only */}}
```

Requests run concurrently, so **responses can arrive out of order**. Match them by `id`.

Requests for a `connectionId` whose `connect` is still in progress (that is, sent after it on
stdin) wait for it. If the connect fails, they fail with its error. Requests for an id that never
had a `connect` fail immediately with `not_connected`.

A line that is not valid JSON gets the answer `{"id": null, "error": {"code": "parse_error"}}`.

Error codes: `cancelled`, `not_connected`, `invalid_params`, `unknown_method`, `parse_error`,
`change_failed`, `internal`, or the database's own code (a SQLSTATE such as `42P01`, or a SQL
Server error number such as `208`).

## Methods

| method | params | result |
|---|---|---|
| `ping` | none | `"pong"` |
| `connect` | `connectionId, engine` (`postgres` \| `mysql` \| `mariadb` \| `sqlite` \| `mssql`), `host?, port?, user?, password?, database?, file?` (sqlite), `ssl?` (`disable` \| `prefer` \| `require`, default `prefer`), `trustServerCertificate?` (mssql), `url?` (sqlx engines, overrides the fields) | `{serverVersion, defaultDatabase, engine}` |
| `disconnect` | `connectionId` | `null` |
| `listDatabases` | `connectionId` | `["name", ...]` |
| `listSchemas` | `connectionId, database?` | `["name", ...]` (`[]` for mysql/mariadb/sqlite) |
| `listObjects` | `connectionId, database?, schema?` | `[{name, schema, kind: "table"\|"view"}]`, sorted by kind, then name |
| `describe` | `connectionId, database?, schema?, table` | `{columns: [{name, type, nullable, default, primaryKey, autoIncrement}], primaryKey: [...]}` |
| `query` | `connectionId, database?, sql, maxRows?` (1000), `requestId?` | `{resultSets: [{columns: [{name,type}], rows, truncated, rowsAffected}], elapsedMs}` |
| `fetchTable` | `connectionId, database?, schema?, table, offset, limit, orderBy?: [{column, desc}], where?, requestId?` | `{columns, rows, total}` |
| `applyChanges` | `connectionId, database?, schema?, table, changes: [{kind:"update",key,values} \| {kind:"delete",key} \| {kind:"insert",values}]` | `{applied}` (the number of changes) |
| `cancel` | `requestId` (string or number) | `{cancelled: bool}` |

Notes:

- **Connections.** Each `connectionId` keeps a pool per database, created lazily: sqlx pools for
  pg/mysql, a small tiberius pool for SQL Server, and a single connection for SQLite (so `ATTACH`
  persists). Connecting again with an existing id replaces the old connection. Default ports are
  5432, 3306 and 1433. For SQL Server, `host` may be `server\instance`, which is resolved through
  SQL Browser. SQLite never creates a missing file.
- **Defaults for `database` and `schema`.** For mysql, `schema` falls back to `database`. For
  sqlite, `schema` or `database` names an attached database, and the default is `main`. For pg,
  the default schema is `current_schema()`, and for SQL Server it is `SCHEMA_NAME()`. Empty
  strings count as absent.
- **`query`.**
  - SQL runs through the driver's raw/simple protocol, so multiple statements and result sets work.
  - A statement without rows gives a result set with `columns: []` and `rowsAffected`.
  - Once a result set passes `maxRows`, it is marked `truncated`, reading stops, and the
    connection is closed instead of reused. This also means later statements in the same script
    are not read.
- **`fetchTable`.**
  - The query is built with quoted identifiers (`"x"`, `` `x` ``, `[x]`), and `where` is inserted
    verbatim as `WHERE (<where>)`.
  - Paging uses `LIMIT/OFFSET`. SQL Server uses `OFFSET … FETCH NEXT`, with
    `ORDER BY (SELECT NULL)` when no order is given.
  - `total` comes from `COUNT(*)` with the same `where` (`COUNT_BIG(*)` on SQL Server).
- **`applyChanges`.**
  - All changes run in one transaction with bound parameters.
  - PostgreSQL binds every value as text and wraps it as `CAST($n AS <column type>)`, with type
    modifiers removed so values are never silently truncated. The other engines rely on implicit
    conversion.
  - A `null` key value matches with `IS NULL`.
  - An update or delete that does not affect exactly 1 row rolls everything back.
  - On failure the error carries `index` (0-based) and a message such as
    `Change 1 (update) failed, nothing was applied: …`.
- **`cancel`.**
  - It aborts the task running the `query`/`fetchTable` with that `requestId`, and that request
    then answers with `code: "cancelled"`.
  - pg, mysql and mssql close the connection (the server may finish the statement in the
    background).
  - SQLite stops the running statement through a progress handler.

## Cell encoding

| value | JSON |
|---|---|
| NULL | `null` |
| booleans | `true` / `false` |
| integers | a number if \|v\| ≤ 2^53, otherwise a string |
| floats | a number; NaN and ±Infinity become `"NaN"`, `"Infinity"`, `"-Infinity"` |
| decimal, numeric, money | a string |
| date, time, timestamp | an ISO-8601 string, e.g. `2024-01-02T03:04:05`, `…+02:00` |
| uuid, json/jsonb, xml, intervals, arrays, enums | their text form |
| binary | `"0x…"` hex of the first 256 bytes, followed by `…` when longer |
| undecodable | `"<unsupported: TYPE>"` |

Each column's `type` is the driver's type name: sqlx's names for pg/mysql/sqlite (`INT4`,
`VARCHAR`, `BOOLEAN`, …) and SQL Server names (`int`, `nvarchar`, …). In `describe`, `type` is
the full declared type (`character varying(50)`, `decimal(10,2)`, …).

## Known limitations

- SQL Server: tiberius does not expose DONE row counts, so statements without result sets give
  no result set. A batch without any result set returns one empty set with `rowsAffected: null`.
- When a script has several statements, a `SELECT` that returns no rows cannot report its columns
  through the raw protocol, so it looks like a command (`rowsAffected: 0`). For a single
  statement, the columns are fetched with a prepare/describe.
- `query` runs on a pooled connection, so session state (`SET`, `USE`, temp tables, an open
  `BEGIN`) does not carry over between requests. SQLite is the exception, because it uses one
  connection.
- SQLite `rowsAffected` after DDL comes from `sqlite3_changes()`, which may be stale.
