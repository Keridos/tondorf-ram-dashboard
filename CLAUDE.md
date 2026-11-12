# RAM Dashboard (wuppp)

REST service in Rust + Rocket. Hosts report free/total RAM, username and
hostname; a frontend reads the hosts ranked by free RAM as JSON. Every report
is also kept in a SQLite history.

## Commands

There is no global Rust toolchain on this NixOS machine; wrap commands in
`nix-shell -p cargo rustc gcc clippy rustfmt --run '...'` (gcc builds the bundled SQLite).

```sh
./run.sh                  # cargo run with DB at ~/.ramdashboard/data.sqlite; works from any cwd, args go to cargo
cargo run                 # serves on port 63151 (from Rocket.toml), writes ./wuppp.db
cargo test                # 18 tests, ~2.5s (one test sleeps to exercise the sweeper)
cargo clippy --all-targets
cargo fmt
```

- Spec: `GET /api/openapi.json` (generated from code, there is no handwritten spec)
- Swagger UI: `/api/swagger-ui/`

## Layout

| File | Contents |
|---|---|
| `src/main.rs` | `AppConfig` → validated `Settings`, `build()`/`build_with(figment)`, fairings (store/DB init, sweeper, CORS), JSON catcher, integration tests |
| `src/api.rs` | Routes, custom responders with OpenAPI docs, `spec_info()` |
| `src/auth.rs` | `WriteAuth` request guard (bearer token, constant-time compare) + its OpenAPI security scheme |
| `src/history.rs` | SQLite `History` (`Mutex<rusqlite::Connection>`): record, query, prune, unit tests |
| `src/model.rs` | Request/response types with `JsonSchema`, validation, hostname normalization |
| `src/store.rs` | In-memory `Store` (`RwLock<HashMap>`), ranking, stale filtering/pruning, unit tests |
| `run.sh` | Launcher: cds to the repo, sets `ROCKET_DATABASE_PATH=~/.ramdashboard/data.sqlite`, runs cargo (via nix-shell if cargo isn't on PATH) |
| `Rocket.toml` | `port`, `stale_after_secs`, `sweep_interval_secs`, `api_token`, `database_path`, `history_retention_days` |

## API

| Method | Path | Auth | Codes |
|---|---|---|---|
| PUT | `/api/v1/hosts/{hostname}` body `{username, free_ram_bytes, total_ram_bytes}` | token | 201 new/re-appeared, 200 updated, 401, 422 |
| GET | `/api/v1/hosts/{hostname}` | | 200, 404, 422 |
| DELETE | `/api/v1/hosts/{hostname}` | token | 204, 401, 404, 422 |
| GET | `/api/v1/hosts/{hostname}/history?since=RFC3339&limit=N` | | 200, 422, 500 |
| GET | `/api/v1/dashboard?limit=N` | | 200, 422 |
| GET | `/api/health` | | 200 |

Rules:
- **Auth:** PUT and DELETE need `Authorization: Bearer <api_token>`. There is one global token, `tondorf.is.cool` (the user chose it), set in `Rocket.toml` / `ROCKET_API_TOKEN`. An empty token fails startup. Reads are open. Auth failures go through the catcher: `401 {"error":"unauthorized"}`.
- **Memory and time:** memory values are bytes (u64), and timestamps are RFC 3339 UTC with microsecond precision.
- **Hostnames:** 1-253 chars of `[A-Za-z0-9._-]`, case-insensitive, stored lowercased.
- **Validation:** username must be non-empty after trimming and ≤256 bytes; `0 < total ≤ i64::MAX`; `free <= total`; unknown JSON fields are rejected.
- **Ranking:** `free_ram_bytes` descending, ties broken by hostname ascending, `rank` starts at 1.
- **Errors:** always `{"error": "..."}`, including Rocket's own catcher responses.
- **`last_seen`:** set by the server when the report arrives.
- **Stale hosts:** a host that hasn't reported for more than `stale_after_secs` (default 300) is hidden from all reads immediately. A background task on liftoff deletes stale hosts every `sweep_interval_secs` (default 30). Both must be ≥1, or startup fails.
- **History:** every accepted PUT is appended to the `reports` table in `database_path` (default `wuppp.db`, relative to the working directory). The history endpoint returns entries newest first; `limit` defaults to 100, range 1-10000. Unknown hosts give an empty list, not 404. History survives staleness, DELETE and restarts. The sweeper deletes rows older than `history_retention_days` (default 30, 0 = keep forever). If writing history fails, the PUT still succeeds and the error is logged.
- **Live state is still in memory:** the dashboard is empty after a restart until hosts report again. It is not rebuilt from history.
- **Path prefix:** everything (routes, `/api/openapi.json`, `/api/swagger-ui/`) is mounted under `api::BASE_PATH` (`/api`). Rocket has no global prefix setting, so `build_with` passes it as the base of `mount_endpoints_and_merged_docs!`. The spec's paths are relative to that base (`/v1/...`, `/health`) and `servers: [{url: "/api"}]` carries the prefix. Unprefixed paths give the JSON 404.
- **CORS:** every response sends `Access-Control-Allow-Origin: *`.

## Conventions

- **Commit at the end of every turn**, before waiting for user input (the user asked for this). Run tests, clippy and `cargo fmt` first. Commit messages end with a `Co-Authored-By` line.
- Keep the generated spec accurate. If a route's possible status codes change, update the responder types in `api.rs` and the test `openapi_spec_documents_all_routes`.
- Store methods take an explicit `now` so the unit tests don't need sleeps.
- Tests build Rocket from `figment()` in `main.rs` tests, which sets `database_path = ":memory:"`. Don't use `build()` in tests, or they'll write `wuppp.db` into the repo.

## Gotchas

- **Route titles in the spec:** the doc comment must start with `/// # Title` for `rocket_okapi` to use it as the operation summary. Without `# `, everything goes into the description.
- **Status codes in the spec:** `(Status, R)` and `Json<T>` are documented by okapi as a single 200/"default" response. Use custom responders that implement `OpenApiResponderInner` (`Reported`, `Invalid`, `ApiError`, `HistoryError`). The 401 comes from `WriteAuth`'s `OpenApiFromRequest::get_responses`.
- **Error types:** `Invalid` (422), `ApiError` (404 + 422) and `HistoryError` (422 + 500) are separate types so each route documents only the codes it can return.
- **Edition 2024:** the okapi module `gen` must be written `rocket_okapi::r#gen`.
- **Testing startup failures:** an unhandled `rocket::Error` panics when dropped. Tests that expect launch to fail must call `e.kind()`.
- **`?limit=` parsing:** query `Option<T>` is infallible in Rocket, so `?limit=abc` is silently treated as no limit (or as the default 100 for history). That's why the ranges are checked by hand, and why the spec shows minimum 0.
- **rusqlite and u64:** rusqlite 0.42 needs the `fallible_uint` feature for u64. SQLite integers are i64, which is why `total_ram_bytes` is capped at `i64::MAX`.
- **Timestamps in SQLite:** stored as fixed-width RFC 3339 text (`...123456Z`), so string order is time order. `last_seen` is truncated to microseconds so the live API and the history agree.
- **`PRAGMA journal_mode`** returns a row, so it must go through `query_row`, not `execute`.
- **Blocking DB calls:** SQLite calls run directly in the (sync) route handlers. That's fine at this scale (WAL, `synchronous = NORMAL`). The sweeper's prune uses `spawn_blocking`.
- **Config lookup:** `Rocket.toml` is found by searching the working directory and its parents. When started from elsewhere, the port falls back to 8000, but the token and DB path keep their code defaults. Use `ROCKET_PORT` or `ROCKET_CONFIG`.
- **Bind address:** Rocket binds to 127.0.0.1 by default. Set `ROCKET_ADDRESS=0.0.0.0` to accept remote clients.

## Current state

- **Features:** all of the above is implemented, tested (18 passing) and clippy-clean.
- **Server copy:** the repo is copied to `y7v.de:~/wuppp`. That server has git but **no Rust toolchain**, so the service isn't built or running there yet.

## Roadmap / open items

1. **Deploy to y7v.de:** install rustup there, or cross-copy a release binary. Set `address = "0.0.0.0"`, put `wuppp.db` somewhere persistent (`database_path`). Optionally add a systemd unit.
2. **Restrict CORS** to the frontend's origin instead of `*`.
3. **Spec polish:** a hostname `pattern` and `limit` minimum 1 would need post-processing of the generated spec.
