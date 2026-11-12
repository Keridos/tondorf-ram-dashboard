# RAM Dashboard (wuppp)

REST service in Rust + Rocket. Hosts report free/total RAM, username and
hostname; a frontend reads the hosts ranked by free RAM as JSON.

## Commands

```sh
cargo run                 # serves on port 63151 (from Rocket.toml)
cargo test                # 11 tests, ~2.5s (one test sleeps to exercise the sweeper)
cargo clippy --all-targets
```

- Spec: `GET /openapi.json` (generated from code, there is no handwritten spec)
- Swagger UI: `/swagger-ui/`

## Layout

| File | Contents |
|---|---|
| `src/main.rs` | `AppConfig` (stale settings), `build()`/`build_with(figment)`, fairings (store init, sweeper, CORS), JSON catcher, integration tests |
| `src/api.rs` | Routes, custom responders with OpenAPI docs, `spec_info()` |
| `src/model.rs` | Request/response types with `JsonSchema`, validation, hostname normalization |
| `src/store.rs` | In-memory `Store` (`RwLock<HashMap>`), ranking, stale filtering/pruning, unit tests |
| `Rocket.toml` | `port`, `stale_after_secs`, `sweep_interval_secs` |

## API

| Method | Path | Codes |
|---|---|---|
| PUT | `/api/v1/hosts/{hostname}` body `{username, free_ram_bytes, total_ram_bytes}` | 201 new/re-appeared, 200 updated, 422 |
| GET | `/api/v1/hosts/{hostname}` | 200, 404, 422 |
| DELETE | `/api/v1/hosts/{hostname}` | 204, 404, 422 |
| GET | `/api/v1/dashboard?limit=N` | 200, 422 |
| GET | `/health` | 200 |

Rules:
- **Memory and time:** memory values are bytes (u64), and timestamps are RFC 3339 UTC.
- **Hostnames:** 1-253 chars of `[A-Za-z0-9._-]`, case-insensitive, stored lowercased.
- **Validation:** username must be non-empty after trimming and ≤256 bytes; `total > 0`; `free <= total`; unknown JSON fields are rejected.
- **Ranking:** `free_ram_bytes` descending, ties broken by hostname ascending, `rank` starts at 1.
- **Errors:** always `{"error": "..."}`, including Rocket's own catcher responses.
- **`last_seen`:** set by the server when the report arrives.
- **Stale hosts:** a host that hasn't reported for more than `stale_after_secs` (default 300) is hidden from all reads immediately. A background task on liftoff deletes stale hosts every `sweep_interval_secs` (default 30). Both must be ≥1, or startup fails.
- **CORS:** every response sends `Access-Control-Allow-Origin: *`.

## Conventions

- **Commit at the end of every turn**, before waiting for user input (the user asked for this). Run tests and clippy first. Commit messages end with a `Co-Authored-By` line.
- Keep the generated spec accurate. If a route's possible status codes change, update the responder types in `api.rs` and the test `openapi_spec_documents_all_routes`.
- Store methods take an explicit `now` so the unit tests don't need sleeps.

## Gotchas

- **Route titles in the spec:** the doc comment must start with `/// # Title` for `rocket_okapi` to use it as the operation summary. Without `# `, everything goes into the description.
- **Status codes in the spec:** `(Status, R)` and `Json<T>` are documented by okapi as a single 200/"default" response. Use custom responders that implement `OpenApiResponderInner` (`Reported`, `Invalid`, `ApiError`).
- **Error types:** `Invalid` (422 only) and `ApiError` (404 + 422) are separate types so each route documents only the codes it can return.
- **Edition 2024:** the okapi module `gen` must be written `rocket_okapi::r#gen`.
- **Testing startup failures:** an unhandled `rocket::Error` panics when dropped. Tests that expect launch to fail must call `e.kind()`.
- **`?limit=` parsing:** query `Option<T>` is infallible in Rocket, so `?limit=abc` is silently treated as no limit. That's why `limit=0` is checked by hand, and why the spec shows minimum 0.
- **Config lookup:** `Rocket.toml` is found by searching the working directory and its parents. When started from elsewhere, the port falls back to 8000. Use `ROCKET_PORT` or `ROCKET_CONFIG`.
- **Bind address:** Rocket binds to 127.0.0.1 by default. Set `ROCKET_ADDRESS=0.0.0.0` to accept remote clients.

## Current state

- **Features:** all of the above is implemented, tested (11 passing) and clippy-clean.
- **Server copy:** the repo is copied to `y7v.de:~/wuppp`. That server has git but **no Rust toolchain**, so the service isn't built or running there yet.

## Roadmap / open items

1. **Deploy to y7v.de:** install rustup there, or cross-copy a release binary. Set `address = "0.0.0.0"`. Optionally add a systemd unit.
2. **Auth for writes:** e.g. a shared API token header on PUT/DELETE. Currently anyone who can reach the service can report or delete hosts.
3. **Restrict CORS** to the frontend's origin instead of `*`.
4. **Persistence:** state is lost on restart. Probably fine since hosts re-report, but SQLite is an option.
5. **Spec polish:** a hostname `pattern` and `limit` minimum 1 would need post-processing of the generated spec.
