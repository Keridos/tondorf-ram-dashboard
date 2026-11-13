# RAM Dashboard (wuppp)

REST service in Rust + Rocket. Hosts report free/total RAM, username and
hostname; a frontend reads the hosts ranked by free RAM as JSON. Every report
is also kept in a SQLite history.

## Commands

There is no global Rust toolchain on this NixOS machine; wrap commands in
`nix-shell -p cargo rustc gcc clippy rustfmt --run '...'` (gcc builds the bundled SQLite).

```sh
./run.sh                  # cargo run with DB at ~/.ramdashboard/data.sqlite; works from any cwd, args go to cargo
./restart.sh              # build, then (re)start detached on 63151; pid+log in ~/.ramdashboard/server.{pid,log}
cargo run                 # serves on port 63151 (from Rocket.toml), writes ./wuppp.db
cargo test                # 30 tests, ~2.5s (one test sleeps to exercise the sweeper)
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
| `src/history.rs` | SQLite `History` (`Mutex<rusqlite::Connection>`): record, query, prune, user colors (`users` table, random generation), unit tests |
| `static/img/` | Pictures for the page, served at `/img/` (`FileServer`); `sam-sad.jpg` / `sam-happy.jpg` (JPEG only) go here (see its README). The directory must exist, or startup panics |
| `static/index.html` | The dashboard page (plain HTML/CSS/JS, no build step), embedded with `include_str!` and served at `/` |
| `src/model.rs` | Request/response types with `JsonSchema`, validation, hostname normalization |
| `src/store.rs` | In-memory `Store` (`RwLock<HashMap<(username, hostname), Host>>`), ranking, stale filtering/pruning, unit tests |
| `run.sh` | Launcher: cds to the repo, sets `ROCKET_DATABASE_PATH=~/.ramdashboard/data.sqlite`, runs cargo (via nix-shell if cargo isn't on PATH) |
| `restart.sh` | Builds first (a failed build leaves the old server running), then stops the instance in `~/.ramdashboard/server.pid` (whole process group) and starts the built `target/debug/ram-dashboard` directly, detached via `setsid`, with `ROCKET_DATABASE_PATH=~/.ramdashboard/data.sqlite`; waits for `/api/health`. Downtime is ~65 ms (it was ~1.5 s when the new process went through `run.sh`/nix-shell/cargo) |
| `Rocket.toml` | `port`, `stale_after_secs`, `sweep_interval_secs`, `api_tokens`, `database_path`, `history_retention_days` |

## API

| Method | Path | Auth | Codes |
|---|---|---|---|
| PUT | `/api/v1/report/{username}/{hostname}` body `{free_ram_bytes, total_ram_bytes, color?, timeout_minutes?}` | token | 201 new/re-appeared, 200 updated, 401, 422 |
| GET | `/api/v1/report/{username}/{hostname}` | | 200, 404, 422 |
| DELETE | `/api/v1/report/{username}/{hostname}` | token | 204, 401, 404, 422 |
| GET | `/api/v1/report/{username}/{hostname}/history?since=RFC3339&limit=N` | | 200, 422, 500 |
| GET | `/api/v1/dashboard?limit=N` | | 200, 422 |
| GET | `/api/health` | | 200 |
| GET | `/` | | 200, the dashboard page; not in the spec |
| GET | `/img/{file}` | | 200 or 404, files from `static/img/`; not in the spec |

Rules:
- **Auth:** PUT and DELETE need `Authorization: Bearer <token>`, where the token is any one of `api_tokens` (values chosen by the user), set in `Rocket.toml` / `ROCKET_API_TOKENS='["a","b"]'`. All tokens are compared in constant time, without stopping at a match. An empty list or an empty token fails startup. Reads are open. Auth failures go through the catcher: `401 {"error":"unauthorized"}`.
- **Memory and time:** memory values are bytes (u64), and timestamps are RFC 3339 UTC with microsecond precision.
- **Identity:** a host is `{username}/{hostname}`. Every per-host route lives under `/v1/report/{username}/{hostname}` (PUT, GET, DELETE, plus `/history`), and the same machine reported by two users is two separate dashboard entries with separate histories. The response of `history` carries `username` and `hostname` at the top level, not per entry.
- **Hostnames:** 1-253 chars of `[A-Za-z0-9._-]`, case-insensitive, stored lowercased.
- **Validation:** the username (path segment, percent-decoded, case-sensitive) must be non-empty after trimming and ≤256 bytes; an empty segment (`/report//h`) never matches the route and gives 404; `0 < total ≤ i64::MAX`; `free <= total`; unknown JSON fields are rejected.
- **Ranking:** `free_ram_bytes` descending, ties broken by hostname then username ascending, `rank` starts at 1.
- **Errors:** always `{"error": "..."}`, including Rocket's own catcher responses.
- **`last_seen`:** set by the server when the report arrives.
- **Stale hosts:** a host that hasn't reported for more than `stale_after_secs` (code default 300; `Rocket.toml` sets 600, i.e. 10 minutes, as the user asked) is hidden from all reads immediately. A report may set `timeout_minutes` (1-60, else 422) to use that instead for this host, for clients that can't report often; it is stored on the host (returned as `timeout_minutes`, `null` = default) and applies until the next report, so a report without it goes back to the default. The sweeper honors it too. A background task on liftoff deletes stale hosts every `sweep_interval_secs` (default 30). Both must be ≥1, or startup fails.
- **History:** every accepted PUT is appended to the `reports` table in `database_path` (default `wuppp.db`, relative to the working directory). The history endpoint returns entries newest first; `limit` defaults to 100, range 1-10000. Unknown hosts give an empty list, not 404. History survives staleness, DELETE and restarts. The sweeper deletes rows older than `history_retention_days` (default 30, 0 = keep forever). If writing history fails, the PUT still succeeds and the error is logged.
- **Live state is still in memory:** the dashboard is empty after a restart until hosts report again. It is not rebuilt from history.
- **Path prefix:** everything except the dashboard page at `/` (routes, `/api/openapi.json`, `/api/swagger-ui/`) is mounted under `api::BASE_PATH` (`/api`). Rocket has no global prefix setting, so `build_with` passes it as the base of `mount_endpoints_and_merged_docs!`. The spec's paths are relative to that base (`/v1/...`, `/health`) and `servers: [{url: "/api"}]` carries the prefix. Unprefixed paths give the JSON 404.
- **User colors:** each user has one color (`#rrggbb`), stored in the `users` table of the same SQLite file and returned as `color` on every host. A report may send `"color": "#rrggbb"` (case-insensitive, stored lowercased; `null` = none) to set or change it, which also recolors the user's other hosts in memory. Without it, a new user gets a random color: random hue at fixed saturation 0.75 / lightness 0.55, picked from 12 random candidates as the one farthest in hue from existing users' colors. Colors are kept forever (not pruned) and survive restarts. If the color lookup fails, the report still succeeds with the requested or an unsaved random color.
- **Frontend:** `static/index.html`, after `frontend-spec.png` (the mockup's colors were placeholders, its greys are kept: page `#404040`, row frame `#5e5e5e`, used segment `#646464`). A centered title "Ram Altman" (also the browser tab title, the user's choice), flanked by a sad picture + `free()` on the left and `malloc()` + a happy picture on the right (round, JPEGs loaded from `img/sam-sad.jpg` / `img/sam-happy.jpg` (`.jpeg` also tried), with an emoji placeholder if none exists; the user supplies the pictures, there are none in the repo), above two columns, Users and Hosts, sorted by free RAM. Each row is a bar: user color = free RAM, grey = used RAM, the rest empty; bar length = total RAM, on one shared scale for both columns (the largest user total = full width), so a pixel is the same number of bytes on both sides. The Users column sums each user's hosts. A hostname reported by several users is labeled `host · user`. Each row shows free/total RAM on the right in GiB with one decimal (`8.4G/16.0G`), at 0.72× the label size in `#dedede` (dimmer than the white names, brighter than any other grey), with an outline scaled to that size via the `--outline` custom property; a long name is cut with an ellipsis before it reaches the number. Labels are white with a black outline on every bar color, built from eight offset `text-shadow`s (`-webkit-text-stroke` drew spikes at sharp glyph corners). Hover/focus shows a tooltip with GiB, % and last report. It polls `./api/v1/dashboard` every 5 s; `api::frontend` substitutes `__API_BASE__` in the HTML with `BASE_PATH`, and the leading `.` keeps it working below a proxy sub-path. **Auto-reload:** `api::frontend_version()` (hash of the page source + `BASE_PATH`) is embedded as `VERSION` and sent as `X-Frontend-Version` on every response (fairing in `build_with`); the page calls `location.reload()` on a mismatch, at most once a minute (sessionStorage guard), so open tabs pick up a new frontend within ~5 s of a restart and builds all text with `textContent` (usernames are untrusted).
- **CORS:** every response sends `Access-Control-Allow-Origin: *`.

## Conventions

- **Commit at the end of every turn**, before waiting for user input (the user asked for this). Run tests, clippy and `cargo fmt` first. Commit messages end with a `Co-Authored-By` line.
- **Restart the service after implementing something** (the user asked for this): run `./restart.sh` after the commit; it only swaps the process once the build has finished, so keep it that way to keep downtime short. It runs on port 63151 with the DB at `~/.ramdashboard/data.sqlite`.
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
- **Screenshotting the frontend:** `nix-shell -p chromium dejavu_fonts`, but headless Chromium renders no text at all unless `FONTCONFIG_FILE` points to a fonts.conf with a `<dir>` for the DejaVu store path. Use a throwaway instance (`ROCKET_PORT=63999 ROCKET_DATABASE_PATH=/some/tmp.sqlite target/debug/ram-dashboard`) so demo users don't get colors in the real DB.
- **Bind address:** `Rocket.toml` sets `address = "127.0.0.1"` on purpose: a reverse proxy in front forwards public traffic. Don't bind to 0.0.0.0.

## Current state

- **Features:** all of the above is implemented, tested (30 passing) and clippy-clean.
- **Server copy:** the repo is copied to `~/wuppp` on the deployment server. That server has git but **no Rust toolchain**, so the service isn't built or running there yet.

## Roadmap / open items

1. **Deploy to the server:** install rustup there, or cross-copy a release binary. Keep `address = "127.0.0.1"` behind the reverse proxy, put `wuppp.db` somewhere persistent (`database_path`). Optionally add a systemd unit.
2. **Restrict CORS:** the bundled frontend is same-origin and doesn't need it; `*` only matters for other frontends.
3. **Spec polish:** a hostname `pattern` and `limit` minimum 1 would need post-processing of the generated spec.
