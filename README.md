# PostgREST (self-contained edition)

A tiny HTTP-to-PostgreSQL gateway written in Rust. It works like PostgREST —
you send a request, you get rows back as JSON — but with one key difference:

> **Every secret is passed *inside* the JSON request, alongside the SQL query.**
> Host, port, database, user, password, and the full mTLS material
> (`sslcert`, `sslkey`, `sslrootcert`) all travel in the request body.

There is no config file, no startup connection string, no secrets on disk. Each
request is fully self-contained and can target a different database with
different credentials.

Connections are kept alive in RAM and reused across requests — one persistent
connection per unique credential set, similar in spirit to Cloudflare Hyperdrive.

---

## Why

PostgREST reads its credentials **once at startup** from a config file and has
no concept of per-request credentials — so it can't accept mTLS certs that vary
per caller. This service flips that model: the caller supplies everything,
including the three mTLS PEMs and the `sslmode`, in the request itself.

---

## Features

- **Self-contained requests** — connection details + TLS certs + SQL in one JSON body.
- **Full mTLS** — client cert, client key, and root CA passed as PEM strings.
- **In-memory connection reuse** — live connections cached per credential set (max 100), never written to disk.
- **Multi-tenant** — different callers with different credentials hold independent live connections at the same time.
- **JSON output** — rows returned as a JSON array, like PostgREST.
- **Parameterized queries** — optional `$1, $2, …` placeholders with a `params` array.

---

## Requirements

- Rust (edition 2024 — toolchain 1.85+; tested on 1.96).
- OpenSSL development libraries (provided by the `openssl` crate's system linkage).

## Build & Run

```bash
cargo build --release
./target/release/PostgREST
```

The server listens on `127.0.0.1:3000` by default.

### CLI Arguments

| Argument | Short | Description |
| :--- | :--- | :--- |
| `--daemon` | `-d` | Install and start the application as a background service. |
| `--disable` | | Stop and uninstall the background service. |
| `--help` | `-h` | Print help information. |
| `--version` | `-V` | Print version information. |

---

## Service Management

PostgREST can be registered as a native background service on both Linux and Windows. This ensures it starts automatically after a reboot and runs without an open terminal.

### Linux (systemd)
The `--daemon` flag creates a systemd unit at `/etc/systemd/system/postgrest-server.service`, enables it, and starts it.
```bash
sudo ./target/release/PostgREST --daemon
```

To check the status:
```bash
systemctl status postgrest-server
```

To remove the service:
```bash
sudo ./target/release/PostgREST --disable
```

### Windows (Service Control Manager)
The `--daemon` flag registers a service named `postgrest-server` in the Windows Service Control Manager.
```powershell
# Run terminal as Administrator
.\target\release\PostgREST.exe --daemon
```

To check the status (PowerShell):
```powershell
Get-Service postgrest-server
```

To remove the service:
```powershell
.\target\release\PostgREST.exe --disable
```

*Note: Administrative/Root privileges are required to modify system services.*

---

## API

### `POST /query`

Request body (JSON):

| Field         | Type            | Required | Description                                                               |
|---------------|-----------------|----------|---------------------------------------------------------------------------|
| `host`        | string          | yes      | PostgreSQL host (trimmed).                                                |
| `port`        | number          | no       | Port (default `5432`).                                                   |
| `dbname`      | string          | yes      | Database name (trimmed).                                                  |
| `user`        | string          | yes      | Username (trimmed).                                                       |
| `password`    | string          | no       | Password (trailing `\n` or `\r` automatically stripped).                 |
| `sslmode`     | string          | no       | See the [SSL Mode implementation table](#ssl-mode-implementation).        |
| `sslrootcert` | string (PEM)    | no       | Root CA certificate, full PEM contents.                                   |
| `sslcert`     | string (PEM)    | no       | Client certificate, full PEM contents (mTLS).                            |
| `sslkey`      | string (PEM)    | no       | Client private key, full PEM contents (mTLS).                            |
| `query`       | string          | yes      | SQL to execute.                                                           |
| `params`      | array           | no       | Bind parameters for `$1, $2, …` placeholders.                            |

**Success** → `200 OK`

```json
{ "rows": [ { "col": "value" }, ... ] }
```

**Error** → `400 Bad Request`

```json
{ "error": "full error chain message" }
```

---

## SSL Mode Implementation

The `sslmode` parameter maps to specific wire-level negotiation and OpenSSL certificate validation behaviors.

| `sslmode` | Wire Mode | CA Verification (`sslrootcert`) | Hostname Verification | Notes |
| :--- | :--- | :---: | :---: | :--- |
| `disable` | `disable` | No | No | Plaintext only. |
| `allow` | `prefer` | No | No | Approximated via `prefer`. |
| `prefer` | `prefer` | No | No | (Default) Try TLS, fall back to plaintext. |
| `require` | `require` | No | No | Force TLS; ignore CA chain. |
| **`verify-ca`** | `require` | **Yes** | No | Force TLS; verify CA chain but skip hostname match. |
| **`verify-full`** | `require` | **Yes** | **Yes** | Force TLS; verify CA chain AND ensure hostname matches certificate. |

Note: If `verify-ca` or `verify-full` is used, you **must** provide the `sslrootcert`. The system roots are also loaded, but the provided PEM is added as the primary trust anchor.

---

## Examples

These assume the PEM files and a `pw.txt` live in a local `.secrets/` directory.

### 1. Parameterized query with mTLS (`verify-ca`)

```bash
curl -s -X POST http://localhost:3000/query \
  -H "Content-Type: application/json" \
  -d "{
    \"host\": \"your.db.host.com\",
    \"port\": 5432,
    \"dbname\": \"your_db\",
    \"user\": \"your_user\",
    \"password\": $(jq -Rs . < .secrets/pw.txt),
    \"sslmode\": \"verify-ca\",
    \"sslrootcert\": $(jq -Rs . < .secrets/root_cert.pem),
    \"sslcert\": $(jq -Rs . < .secrets/client_cert.pem),
    \"sslkey\": $(jq -Rs . < .secrets/client_key.pem),
    \"query\": \"SELECT * FROM users WHERE id = \$1\",
    \"params\": [\"123\"]
  }" | jq .
```

### 2. Plaintext connection (no TLS)

```bash
curl -s -X POST http://localhost:3000/query \
  -H "Content-Type: application/json" \
  -d '{
    "host": "localhost",
    "dbname": "mydb",
    "user": "postgres",
    "password": "postgres",
    "sslmode": "disable",
    "query": "SELECT now() AS ts"
  }' | jq .
```

---

## How connection reuse works

Each request builds a `ConnectionKey` from all connection fields
(host, port, dbname, user, password, sslmode, and the three PEMs). That key
indexes a high-performance **Moka** cache of live clients held in memory:

- **Max Capacity**: 100 concurrent connections.
- **Eviction**: When the limit is reached, the least recently used (LRU) connection is evicted to make room for a new one.
- **Reuse**: Subsequent requests with the same key reuse the live connection.
- **Auto-reconnect**: If a cached connection has dropped (`is_closed()`), it is transparently reopened.
- **Isolation**: Different callers keep **independent** live connections side by side.

---

## Type mapping

Postgres values are converted to JSON by column type:

| Postgres type        | JSON                |
|----------------------|---------------------|
| `bool`               | boolean             |
| `int2`/`int4`/`int8` | number              |
| `float4`/`float8`    | number              |
| `json`/`jsonb`       | object / array      |
| everything else      | string              |
| `NULL`               | `null`              |

---

## Security notes

- **Nothing touches disk.** Certificates and keys are parsed directly from the
  request bytes in memory. No temp files, no logging of secrets.
- **Memory Swapping**: The one OS-level caveat is memory pressure: data in RAM
  could in theory be swapped to disk. Use `mlock` or encrypted swap if that is
  in your threat model.
- This service runs **arbitrary SQL** supplied by the caller with the caller's
  own credentials. Put it behind your own authentication/authorization and
  network controls.
- Always run it over a trusted transport (TLS terminator / private network);
  the request body carries live credentials and private keys.

---

## Project layout

```
.
├── Cargo.toml
├── src/
│   ├── main.rs      # entry point and server logic
│   └── cli.rs       # CLI parsing and service management
└── .secrets/        # your local PEMs + pw.txt (gitignored)
```
