#[macro_use]
extern crate rocket;

mod api;
mod auth;
mod history;
mod model;
mod store;

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use rocket::fairing::AdHoc;
use rocket::figment::Figment;
use rocket::fs::{FileServer, relative};
use rocket::http::{Header, Status};
use rocket::serde::json::Json;
use rocket::{Build, Request, Rocket};
use rocket_okapi::settings::OpenApiSettings;
use rocket_okapi::swagger_ui::{SwaggerUIConfig, make_swagger_ui};
use rocket_okapi::{mount_endpoints_and_merged_docs, openapi_get_routes_spec};
use serde::Deserialize;

use auth::ApiTokens;
use history::History;
use model::ErrorBody;
use store::Store;

/// App settings, read from Rocket.toml or `ROCKET_*` env vars
/// (e.g. `ROCKET_STALE_AFTER_SECS=120`).
#[derive(Debug, Deserialize)]
#[serde(default)]
struct AppConfig {
    /// Hosts that haven't reported for this long are removed.
    stale_after_secs: u64,
    /// How often the background sweeper deletes stale hosts from memory.
    sweep_interval_secs: u64,
    /// Bearer tokens accepted for PUT and DELETE (any one of them).
    api_tokens: Vec<String>,
    /// SQLite file holding the report history (`:memory:` for none).
    database_path: String,
    /// History rows older than this are deleted. 0 keeps history forever.
    history_retention_days: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            stale_after_secs: 300,
            sweep_interval_secs: 30,
            api_tokens: vec!["tondorf.is.cool".into(), "tondorf.ist.cool".into()],
            database_path: "wuppp.db".into(),
            history_retention_days: 30,
        }
    }
}

/// Validated settings.
struct Settings {
    stale_after: TimeDelta,
    sweep: Sweep,
    api_tokens: Vec<String>,
    database_path: String,
}

impl AppConfig {
    fn from_figment(figment: &Figment) -> Result<Settings, String> {
        let cfg: Self = figment.extract().map_err(|e| e.to_string())?;
        let stale_after = i64::try_from(cfg.stale_after_secs)
            .ok()
            .and_then(TimeDelta::try_seconds)
            .filter(|d| *d > TimeDelta::zero())
            .ok_or("stale_after_secs must be between 1 and 9223372036854775")?;
        if cfg.sweep_interval_secs == 0 {
            return Err("sweep_interval_secs must be >= 1".into());
        }
        if cfg.api_tokens.is_empty() {
            return Err("api_tokens must contain at least one token".into());
        }
        if cfg.api_tokens.iter().any(|t| t.trim().is_empty()) {
            return Err("api_tokens must not contain empty tokens".into());
        }
        let retention = match cfg.history_retention_days {
            0 => None,
            days => Some(
                i64::try_from(days)
                    .ok()
                    .and_then(TimeDelta::try_days)
                    .ok_or("history_retention_days is too large")?,
            ),
        };
        Ok(Settings {
            stale_after,
            sweep: Sweep {
                every: Duration::from_secs(cfg.sweep_interval_secs),
                retention,
            },
            api_tokens: cfg.api_tokens,
            database_path: cfg.database_path,
        })
    }
}

#[catch(default)]
fn default_catcher(status: Status, _req: &Request) -> (Status, Json<ErrorBody>) {
    let error = status.reason().unwrap_or("error").to_lowercase();
    (status, Json(ErrorBody { error }))
}

fn build() -> Rocket<Build> {
    build_with(rocket::Config::figment())
}

fn build_with(figment: Figment) -> Rocket<Build> {
    let mut rocket = rocket::custom(figment);
    let settings = OpenApiSettings::default();
    // Mounts the routes plus the merged spec at /api/openapi.json.
    mount_endpoints_and_merged_docs! {
        rocket, api::BASE_PATH, settings,
        "" => (vec![], api::spec_info()),
        "/v1" => openapi_get_routes_spec![
            settings: api::report_host, api::get_host, api::delete_host, api::host_history,
            api::dashboard, api::timeline
        ],
        "" => openapi_get_routes_spec![settings: api::health],
    };

    rocket
        // The dashboard page at the root (outside /api and the spec).
        .mount("/", routes![api::frontend])
        // Pictures for the page (see static/img/README.md).
        .mount("/img", FileServer::from(relative!("static/img")))
        .mount(
            format!("{}/swagger-ui/", api::BASE_PATH),
            make_swagger_ui(&SwaggerUIConfig {
                url: "../openapi.json".to_owned(),
                ..Default::default()
            }),
        )
        .attach(AdHoc::try_on_ignite("Store", |rocket| async move {
            let settings = match AppConfig::from_figment(rocket.figment()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("invalid configuration: {e}");
                    return Err(rocket);
                }
            };
            let history = match History::open(&settings.database_path) {
                Ok(h) => Arc::new(h),
                Err(e) => {
                    eprintln!("cannot open database '{}': {e}", settings.database_path);
                    return Err(rocket);
                }
            };
            let store = Store::new(settings.stale_after);
            restore_live_hosts(&store, &history, settings.stale_after);
            Ok(rocket
                .manage(Arc::new(store))
                .manage(history)
                .manage(ApiTokens(settings.api_tokens))
                .manage(settings.sweep))
        }))
        .attach(AdHoc::on_liftoff("Stale host sweeper", |rocket| {
            Box::pin(async move {
                let store = Arc::clone(rocket.state::<Arc<Store>>().unwrap());
                let history = Arc::clone(rocket.state::<Arc<History>>().unwrap());
                let sweep = *rocket.state::<Sweep>().unwrap();
                rocket::tokio::spawn(async move {
                    let mut tick = rocket::tokio::time::interval(sweep.every);
                    loop {
                        tick.tick().await;
                        let now = Utc::now();
                        store.prune(now);
                        if let Some(retention) = sweep.retention {
                            let history = Arc::clone(&history);
                            let res = rocket::tokio::task::spawn_blocking(move || {
                                history.prune(now - retention)
                            })
                            .await;
                            if let Ok(Err(e)) = res {
                                eprintln!("history pruning failed: {e}");
                            }
                        }
                    }
                });
            })
        }))
        .register("/", catchers![default_catcher])
        // Let a browser-based frontend on another origin read the dashboard.
        .attach(AdHoc::on_response("CORS", |_req, res| {
            Box::pin(async move {
                res.set_header(Header::new("Access-Control-Allow-Origin", "*"));
            })
        }))
        // Lets open dashboard pages notice a new frontend and reload themselves.
        .attach(AdHoc::on_response("Frontend version", |_req, res| {
            Box::pin(async move {
                res.set_header(Header::new(
                    api::FRONTEND_VERSION_HEADER,
                    api::frontend_version(),
                ));
            })
        }))
}

/// Refills the in-memory store from the history, so hosts that reported
/// recently are back on the dashboard right after a restart instead of only
/// after their next report. Failures are logged; the server starts anyway.
fn restore_live_hosts(store: &Store, history: &History, default_timeout: TimeDelta) {
    let now = Utc::now();
    // No host can be live longer ago than the longest possible timeout.
    let lookback = default_timeout.max(TimeDelta::minutes(model::MAX_TIMEOUT_MINUTES.into()));
    let hosts = match history.latest_reports(now - lookback) {
        Ok(hosts) => hosts,
        Err(e) => {
            eprintln!("could not restore hosts from history: {e}");
            return;
        }
    };
    for mut host in hosts {
        if host.color.is_empty() {
            // History from before user colors: give the user one now.
            host.color = history
                .user_color(&host.username, None)
                .unwrap_or_else(|_| history::random_color());
        }
        store.upsert(host);
    }
    // Drop the ones already past their own timeout.
    store.prune(now);
    let restored = store.dashboard(None, now).host_count;
    if restored > 0 {
        println!("restored {restored} live host(s) from history");
    }
}

/// Background sweeper settings.
#[derive(Clone, Copy)]
struct Sweep {
    every: Duration,
    /// History retention; `None` keeps history forever.
    retention: Option<TimeDelta>,
}

#[launch]
fn rocket() -> _ {
    build()
}

#[cfg(test)]
mod tests {
    use super::build_with;
    use crate::store::Store;
    use rocket::error::ErrorKind;
    use rocket::figment::Figment;
    use rocket::http::{ContentType, Header, Status};
    use rocket::local::blocking::Client;
    use rocket::serde::json::{Value, json};
    use std::sync::Arc;

    const TOKEN: &str = "tondorf.is.cool";

    /// Rocket.toml settings, but with a throwaway database.
    fn figment() -> Figment {
        rocket::Config::figment().merge(("database_path", ":memory:"))
    }

    fn client() -> Client {
        Client::tracked(build_with(figment())).expect("valid rocket instance")
    }

    fn auth(token: &str) -> Header<'static> {
        Header::new("Authorization", format!("Bearer {token}"))
    }

    fn report(c: &Client, host: &str, user: &str, free: u64, total: u64) -> Status {
        c.put(format!("/api/v1/report/{user}/{host}"))
            .header(ContentType::JSON)
            .header(auth(TOKEN))
            .body(format!(
                r#"{{"free_ram_bytes":{free},"total_ram_bytes":{total}}}"#
            ))
            .dispatch()
            .status()
    }

    #[test]
    fn upsert_returns_created_then_ok() {
        let c = client();
        assert_eq!(report(&c, "web-01", "alice", 1, 2), Status::Created);
        assert_eq!(report(&c, "WEB-01", "alice", 1, 2), Status::Ok);
    }

    #[test]
    fn dashboard_is_ranked_by_free_ram() {
        let c = client();
        report(&c, "small", "a", 1_000, 4_000);
        report(&c, "big", "b", 9_000, 16_000);
        report(&c, "mid", "c", 5_000, 8_000);

        let body: Value = c.get("/api/v1/dashboard").dispatch().into_json().unwrap();
        let names: Vec<_> = body["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["hostname"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["big", "mid", "small"]);
        assert_eq!(body["hosts"][0]["rank"], 1);
        assert_eq!(body["hosts"][1]["free_ram_percent"], 62.5);
        assert_eq!(body["host_count"], 3);

        let top: Value = c
            .get("/api/v1/dashboard?limit=1")
            .dispatch()
            .into_json()
            .unwrap();
        assert_eq!(top["hosts"].as_array().unwrap().len(), 1);
        assert_eq!(top["host_count"], 3);
    }

    #[test]
    fn rejects_invalid_reports() {
        let c = client();
        assert_eq!(report(&c, "h", "a", 5, 4), Status::UnprocessableEntity);
        assert_eq!(report(&c, "h", "a", 0, 0), Status::UnprocessableEntity);
        assert_eq!(report(&c, "h", "%20", 1, 2), Status::UnprocessableEntity);
        let long_user = "u".repeat(257);
        assert_eq!(
            report(&c, "h", &long_user, 1, 2),
            Status::UnprocessableEntity
        );
        assert_eq!(
            report(&c, "bad%20host", "a", 1, 2),
            Status::UnprocessableEntity
        );

        assert_eq!(
            report(&c, "h", "a", 1, u64::MAX),
            Status::UnprocessableEntity
        );

        let res = c
            .put("/api/v1/report/a/h")
            .header(ContentType::JSON)
            .header(auth(TOKEN))
            .body(r#"{"free_ram_bytes":1}"#)
            .dispatch();
        assert_eq!(res.status(), Status::UnprocessableEntity);
        let body: Value = res.into_json().unwrap();
        assert!(body["error"].as_str().unwrap().contains("missing field"));

        // The username belongs in the path now.
        let res = c
            .put("/api/v1/report/a/h")
            .header(ContentType::JSON)
            .header(auth(TOKEN))
            .body(r#"{"username":"a","free_ram_bytes":1,"total_ram_bytes":2}"#)
            .dispatch();
        assert_eq!(res.status(), Status::UnprocessableEntity);
    }

    #[test]
    fn report_takes_username_and_hostname_from_path() {
        let c = client();
        assert_eq!(report(&c, "Web-01", "alice%20b", 1, 2), Status::Created);
        let host: Value = c
            .get("/api/v1/report/alice%20b/WEB-01")
            .dispatch()
            .into_json()
            .unwrap();
        assert_eq!(host["hostname"], "web-01");
        assert_eq!(host["username"], "alice b");

        // The old routes are gone.
        let res = c
            .put("/api/v1/hosts/web-01")
            .header(ContentType::JSON)
            .header(auth(TOKEN))
            .body(r#"{"free_ram_bytes":1,"total_ram_bytes":2}"#)
            .dispatch();
        assert_eq!(res.status(), Status::NotFound);
        for old in [
            "/api/v1/hosts/web-01",
            "/api/v1/hosts/web-01/history",
            "/api/v1/host/alice%20b/web-01",
            "/api/v1/history/alice%20b/web-01",
        ] {
            assert_eq!(c.get(old).dispatch().status(), Status::NotFound, "{old}");
        }
    }

    #[test]
    fn hosts_are_identified_by_username_and_hostname() {
        let c = client();
        assert_eq!(report(&c, "box", "alice", 10, 100), Status::Created);
        assert_eq!(report(&c, "box", "bob", 20, 100), Status::Created);
        assert_eq!(report(&c, "box", "alice", 30, 100), Status::Ok);

        let dash: Value = c.get("/api/v1/dashboard").dispatch().into_json().unwrap();
        assert_eq!(dash["host_count"], 2);
        assert_eq!(dash["hosts"][0]["username"], "alice");
        assert_eq!(dash["hosts"][0]["free_ram_bytes"], 30);
        assert_eq!(dash["hosts"][1]["username"], "bob");

        // Usernames are case-sensitive, hostnames aren't.
        let get = |path: &str| c.get(path).dispatch().status();
        assert_eq!(get("/api/v1/report/bob/BOX"), Status::Ok);
        assert_eq!(get("/api/v1/report/Bob/box"), Status::NotFound);
        assert_eq!(get("/api/v1/report/carol/box"), Status::NotFound);

        let history: Value = c
            .get("/api/v1/report/alice/box/history")
            .dispatch()
            .into_json()
            .unwrap();
        assert_eq!(history["username"], "alice");
        assert_eq!(history["entries"].as_array().unwrap().len(), 2);

        // Deleting one user's entry leaves the other.
        let status = c
            .delete("/api/v1/report/alice/box")
            .header(auth(TOKEN))
            .dispatch()
            .status();
        assert_eq!(status, Status::NoContent);
        assert_eq!(get("/api/v1/report/alice/box"), Status::NotFound);
        assert_eq!(get("/api/v1/report/bob/box"), Status::Ok);
    }

    #[test]
    fn delete_removes_the_host_and_its_history() {
        let c = client();
        let history = |host: &str| -> usize {
            let v: Value = c
                .get(format!("/api/v1/report/a/{host}/history"))
                .dispatch()
                .into_json()
                .unwrap();
            v["entries"].as_array().unwrap().len()
        };
        let delete = |host: &str| {
            c.delete(format!("/api/v1/report/a/{host}"))
                .header(auth(TOKEN))
                .dispatch()
                .status()
        };
        for free in [1, 2] {
            report(&c, "gone", "a", free, 10);
        }
        report(&c, "kept", "a", 1, 10);
        assert_eq!(history("gone"), 2);

        assert_eq!(delete("GONE"), Status::NoContent);
        assert_eq!(history("gone"), 0);
        assert_eq!(
            c.get("/api/v1/report/a/gone").dispatch().status(),
            Status::NotFound
        );
        assert_eq!(history("kept"), 1, "other hosts untouched");
        assert_eq!(delete("gone"), Status::NotFound, "nothing left");
    }

    #[test]
    fn delete_also_works_for_a_host_that_is_only_in_the_history() {
        // stale_after 1 s: the host drops off the dashboard, its history stays.
        let figment = figment()
            .merge(("stale_after_secs", 1))
            .merge(("sweep_interval_secs", 3600));
        let c = Client::tracked(build_with(figment)).unwrap();
        report(&c, "old", "a", 1, 2);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(
            c.get("/api/v1/report/a/old").dispatch().status(),
            Status::NotFound
        );

        let status = c
            .delete("/api/v1/report/a/old")
            .header(auth(TOKEN))
            .dispatch()
            .status();
        assert_eq!(status, Status::NoContent);
        let v: Value = c
            .get("/api/v1/report/a/old/history")
            .dispatch()
            .into_json()
            .unwrap();
        assert!(v["entries"].as_array().unwrap().is_empty());
    }

    #[test]
    fn get_and_delete_host() {
        let c = client();
        assert_eq!(
            c.get("/api/v1/report/a/x").dispatch().status(),
            Status::NotFound
        );
        report(&c, "x", "a", 1, 2);
        let host: Value = c.get("/api/v1/report/a/X").dispatch().into_json().unwrap();
        assert_eq!(host["username"], "a");
        let delete = || {
            c.delete("/api/v1/report/a/x")
                .header(auth(TOKEN))
                .dispatch()
                .status()
        };
        assert_eq!(delete(), Status::NoContent);
        assert_eq!(delete(), Status::NotFound);
    }

    #[test]
    fn writes_require_the_token() {
        let c = client();
        let put = |headers: Vec<Header<'static>>| {
            let mut req = c
                .put("/api/v1/report/a/h")
                .header(ContentType::JSON)
                .body(r#"{"free_ram_bytes":1,"total_ram_bytes":2}"#);
            for h in headers {
                req = req.header(h);
            }
            req.dispatch()
        };

        for headers in [
            vec![],
            vec![auth("wrong")],
            vec![Header::new("Authorization", TOKEN)], // missing "Bearer "
        ] {
            let res = put(headers);
            assert_eq!(res.status(), Status::Unauthorized);
            assert_eq!(res.content_type(), Some(ContentType::JSON));
            let body: Value = res.into_json().unwrap();
            assert_eq!(body["error"], "unauthorized");
        }
        assert_eq!(put(vec![auth(TOKEN)]).status(), Status::Created);

        // Reads stay open; deletes need the token.
        assert_eq!(c.get("/api/v1/report/a/h").dispatch().status(), Status::Ok);
        assert_eq!(
            c.delete("/api/v1/report/a/h").dispatch().status(),
            Status::Unauthorized
        );
        assert_eq!(c.get("/api/v1/report/a/h").dispatch().status(), Status::Ok);
    }

    #[test]
    fn tokens_are_configurable() {
        let tokens = ["one", "two"];
        let c = Client::tracked(build_with(figment().merge(("api_tokens", tokens)))).unwrap();
        assert_eq!(report(&c, "h", "a", 1, 2), Status::Unauthorized);
        for token in tokens {
            let status = c
                .delete("/api/v1/report/a/h")
                .header(auth(token))
                .dispatch()
                .status();
            assert_eq!(status, Status::NotFound, "{token}");
        }
    }

    #[test]
    fn both_default_tokens_are_accepted() {
        let c = client();
        for token in ["tondorf.is.cool", "tondorf.ist.cool"] {
            let status = c
                .put("/api/v1/report/a/h")
                .header(ContentType::JSON)
                .header(auth(token))
                .body(r#"{"free_ram_bytes":1,"total_ram_bytes":2}"#)
                .dispatch()
                .status();
            assert!(status.class().is_success(), "{token}: {status}");
        }
    }

    #[test]
    fn history_records_every_report() {
        let c = client();
        for free in [10, 20, 30] {
            report(&c, "H", "a", free, 100);
        }
        report(&c, "other", "a", 1, 100);

        let get = |q: &str| c.get(format!("/api/v1/report/a/h/history{q}")).dispatch();
        let body: Value = get("").into_json().unwrap();
        assert_eq!(body["hostname"], "h");
        let frees: Vec<_> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["free_ram_bytes"].as_u64().unwrap())
            .collect();
        assert_eq!(frees, [30, 20, 10], "newest first");

        let top: Value = get("?limit=1").into_json().unwrap();
        assert_eq!(top["entries"].as_array().unwrap().len(), 1);

        let newest = body["entries"][0]["reported_at"].as_str().unwrap();
        let since: Value = get(&format!("?since={newest}")).into_json().unwrap();
        assert_eq!(since["entries"].as_array().unwrap().len(), 1);

        let none: Value = c
            .get("/api/v1/report/a/unknown/history")
            .dispatch()
            .into_json()
            .unwrap();
        assert_eq!(none["entries"].as_array().unwrap().len(), 0);

        for bad in ["?limit=0", "?limit=10001", "?since=yesterday"] {
            assert_eq!(get(bad).status(), Status::UnprocessableEntity, "{bad}");
        }
    }

    #[test]
    fn timeline_sums_free_and_total_ram() {
        let c = client();
        let empty: Value = c.get("/api/v1/timeline").dispatch().into_json().unwrap();
        assert_eq!(empty["span_secs"], 172800);
        assert_eq!(empty["samples"].as_array().unwrap().len(), 120);
        assert!(empty["users"].as_array().unwrap().is_empty());
        assert!(empty["total"]["free_ram_bytes"][119].is_null());

        report(&c, "a", "alice", 10, 100);
        report(&c, "b", "alice", 20, 50);
        report(&c, "c", "bob", 1, 4);
        let tl: Value = c
            .get("/api/v1/timeline?span_secs=600&points=3")
            .dispatch()
            .into_json()
            .unwrap();
        assert_eq!(tl["samples"].as_array().unwrap().len(), 3);
        assert_eq!(tl["samples"][2], tl["generated_at"], "newest sample is now");
        assert_eq!(tl["total"]["free_ram_bytes"], json!([null, null, 31]));
        assert_eq!(tl["total"]["total_ram_bytes"], json!([null, null, 154]));
        assert_eq!(tl["users"][0]["username"], "alice");
        assert_eq!(tl["users"][0]["free_ram_bytes"][2], 30);
        assert_eq!(tl["users"][1]["total_ram_bytes"][2], 4);
        assert!(tl["users"][1]["color"].as_str().unwrap().starts_with('#'));

        for bad in [
            "span_secs=59",
            "span_secs=2592001",
            "points=1",
            "points=1001",
        ] {
            let res = c.get(format!("/api/v1/timeline?{bad}")).dispatch();
            assert_eq!(res.status(), Status::UnprocessableEntity, "{bad}");
        }
    }

    #[test]
    fn unknown_route_returns_json_404() {
        let c = client();
        let res = c.get("/nope").dispatch();
        assert_eq!(res.status(), Status::NotFound);
        assert_eq!(res.content_type(), Some(ContentType::JSON));
    }

    #[test]
    fn invalid_stale_config_fails_to_start() {
        let bad: [(&str, Value); 5] = [
            ("stale_after_secs", 0.into()),
            ("sweep_interval_secs", 0.into()),
            ("api_tokens", Value::Array(vec![])),
            ("api_tokens", vec!["ok", " "].into()),
            ("database_path", "/nonexistent/dir/x.db".into()),
        ];
        for (key, val) in bad {
            let figment = figment().merge((key, &val));
            match Client::tracked(build_with(figment)) {
                Ok(_) => panic!("{key}={val} accepted"),
                // Inspecting the error marks it handled (Rocket panics on unhandled drop).
                Err(e) => assert!(matches!(e.kind(), ErrorKind::FailedFairings(_))),
            }
        }
    }

    #[test]
    fn restart_restores_live_hosts_from_history() {
        use crate::history::History;
        use crate::model::Host;
        use chrono::{SubsecRound, TimeDelta, Utc};

        let dir = std::env::temp_dir().join(format!("wuppp-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("restore.db");
        let db = db.to_str().unwrap().to_owned();
        let with_db = || figment().merge(("database_path", &db));

        // First run: a normal report.
        {
            let c = Client::tracked(build_with(with_db())).unwrap();
            let body = r##"{"free_ram_bytes":1,"total_ram_bytes":2,"color":"#abcdef","timeout_minutes":45}"##;
            let res = c
                .put("/api/v1/report/alice/box")
                .header(ContentType::JSON)
                .header(auth(TOKEN))
                .body(body)
                .dispatch();
            assert_eq!(res.status(), Status::Created);
        }

        // Older history, written directly: one host still inside its own
        // 30 min timeout, one past the 10 min default.
        let now = Utc::now().trunc_subsecs(6);
        let old = |user: &str, mins: i64, timeout| Host {
            hostname: "box".into(),
            username: user.into(),
            free_ram_bytes: 5,
            total_ram_bytes: 10,
            color: String::new(),
            timeout_minutes: timeout,
            last_seen: now - TimeDelta::minutes(mins),
        };
        let history = History::open(&db).unwrap();
        history.record(&old("bob", 20, Some(30))).unwrap();
        history.record(&old("carol", 20, None)).unwrap();
        drop(history);

        // Second run: live hosts are back without reporting again.
        let c = Client::tracked(build_with(with_db())).unwrap();
        let get = |user: &str| c.get(format!("/api/v1/report/{user}/box")).dispatch();
        let alice: Value = get("alice").into_json().unwrap();
        assert_eq!(alice["color"], "#abcdef");
        assert_eq!(alice["timeout_minutes"], 45);
        let bob: Value = get("bob").into_json().unwrap();
        assert_eq!(bob["timeout_minutes"], 30);
        assert!(
            bob["color"].as_str().unwrap().starts_with('#'),
            "got a color"
        );
        assert_eq!(get("carol").status(), Status::NotFound, "past its timeout");
        let dash: Value = c.get("/api/v1/dashboard").dispatch().into_json().unwrap();
        assert_eq!(dash["host_count"], 2);

        // The next report updates rather than "creates" a restored host.
        let status = c
            .put("/api/v1/report/alice/box")
            .header(ContentType::JSON)
            .header(auth(TOKEN))
            .body(r#"{"free_ram_bytes":1,"total_ram_bytes":2}"#)
            .dispatch()
            .status();
        assert_eq!(status, Status::Ok);
        drop(c);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sweeper_removes_stale_hosts() {
        let figment = figment()
            .merge(("stale_after_secs", 1))
            .merge(("sweep_interval_secs", 1));
        let c = Client::tracked(build_with(figment)).unwrap();
        report(&c, "gone", "a", 1, 2);

        let dash: Value = c.get("/api/v1/dashboard").dispatch().into_json().unwrap();
        assert_eq!(dash["host_count"], 1);
        assert_eq!(dash["stale_after_secs"], 1);

        std::thread::sleep(std::time::Duration::from_millis(2500));
        let store = c.rocket().state::<Arc<Store>>().unwrap();
        assert_eq!(store.stored_len(), 0, "sweeper did not prune");
        assert_eq!(
            c.get("/api/v1/report/a/gone").dispatch().status(),
            Status::NotFound
        );
    }

    #[test]
    fn openapi_spec_documents_all_routes() {
        let c = client();
        let spec: Value = c.get("/api/openapi.json").dispatch().into_json().unwrap();
        assert_eq!(spec["info"]["title"], "RAM Dashboard API");

        let codes = |path: &str, method: &str| -> Vec<String> {
            let op = &spec["paths"][path][method];
            assert!(op["summary"].is_string(), "{method} {path} has no summary");
            op["responses"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        let host = "/v1/report/{username}/{hostname}";
        assert_eq!(codes(host, "put"), ["200", "201", "401", "422"]);
        for old in [
            "/v1/hosts/{hostname}",
            "/v1/hosts/{hostname}/history",
            "/v1/host/{username}/{hostname}",
            "/v1/history/{username}/{hostname}",
        ] {
            assert!(spec["paths"][old].is_null(), "{old} still documented");
        }
        assert_eq!(codes(host, "get"), ["200", "404", "422"]);
        assert_eq!(codes(host, "delete"), ["204", "401", "404", "422", "500"]);
        assert_eq!(
            codes("/v1/report/{username}/{hostname}/history", "get"),
            ["200", "422", "500"]
        );
        assert_eq!(codes("/v1/dashboard", "get"), ["200", "422"]);
        assert_eq!(codes("/v1/timeline", "get"), ["200", "422", "500"]);
        assert_eq!(codes("/health", "get"), ["200"]);

        // Only the write routes declare the bearer token.
        let secured = |path: &str, method: &str| spec["paths"][path][method]["security"].is_array();
        assert!(secured(host, "put") && secured(host, "delete"));
        assert!(!secured(host, "get"));
        assert_eq!(
            spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );

        let schemas = &spec["components"]["schemas"];
        for name in [
            "HostReport",
            "Host",
            "RankedHost",
            "Dashboard",
            "ErrorBody",
            "HostHistory",
            "HistoryEntry",
            "Timeline",
            "UserSeries",
        ] {
            assert!(schemas[name].is_object(), "schema {name} missing");
        }
    }

    #[test]
    fn everything_is_under_api_prefix() {
        let c = client();
        for path in [
            "/api/health",
            "/api/openapi.json",
            "/api/swagger-ui/index.html",
        ] {
            assert_eq!(c.get(path).dispatch().status(), Status::Ok, "{path}");
        }
        for path in [
            "/health",
            "/openapi.json",
            "/swagger-ui/index.html",
            "/v1/dashboard",
        ] {
            let res = c.get(path).dispatch();
            assert_eq!(res.status(), Status::NotFound, "{path}");
            assert_eq!(res.into_json::<Value>().unwrap()["error"], "not found");
        }
        let spec: Value = c.get("/api/openapi.json").dispatch().into_json().unwrap();
        assert_eq!(spec["servers"][0]["url"], "/api");
    }

    #[test]
    fn frontend_is_served() {
        let c = client();
        let res = c.get("/").dispatch();
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(res.content_type(), Some(ContentType::HTML));
        let html = res.into_string().unwrap();
        assert!(html.contains(r#"const API = "." + "/api" + "/v1/dashboard";"#));
        assert!(!html.contains("__API_BASE__"));
        assert!(!html.contains("__FRONTEND_VERSION__"));

        // The page embeds the version every response announces.
        let version = crate::api::frontend_version();
        assert_eq!(version.len(), 16);
        assert!(html.contains(&format!(r#"const VERSION = "{version}";"#)));
        let res = c.get("/api/v1/dashboard").dispatch();
        assert_eq!(res.headers().get_one("X-Frontend-Version"), Some(version));
        for path in ["/api/", "/api"] {
            assert_eq!(c.get(path).dispatch().status(), Status::NotFound, "{path}");
        }

        // Picture slots, served from static/img/ (missing files are a JSON 404).
        assert!(
            html.contains(r#"data-name="sam-sad""#) && html.contains(r#"data-name="sam-happy""#)
        );
        assert_eq!(c.get("/img/README.md").dispatch().status(), Status::Ok);
        let res = c.get("/img/does-not-exist.png").dispatch();
        assert_eq!(res.status(), Status::NotFound);
        assert_eq!(res.content_type(), Some(ContentType::JSON));
        let spec: Value = c.get("/api/openapi.json").dispatch().into_json().unwrap();
        assert!(
            spec["paths"]["/"].is_null(),
            "frontend leaked into the spec"
        );
    }

    #[test]
    fn user_colors_are_random_then_kept_or_set() {
        let c = client();
        let put = |user: &str, host: &str, body: &str| {
            c.put(format!("/api/v1/report/{user}/{host}"))
                .header(ContentType::JSON)
                .header(auth(TOKEN))
                .body(body)
                .dispatch()
        };
        let plain = r#"{"free_ram_bytes":1,"total_ram_bytes":2}"#;
        let color_of = |user: &str, host: &str| -> String {
            let v: Value = c
                .get(format!("/api/v1/report/{user}/{host}"))
                .dispatch()
                .into_json()
                .unwrap();
            v["color"].as_str().unwrap().to_owned()
        };

        // A random color on the first report, kept by later ones and other hosts.
        let res: Value = put("alice", "a1", plain).into_json().unwrap();
        let random = res["color"].as_str().unwrap().to_owned();
        assert!(crate::model::normalize_color(&random).is_ok(), "{random}");
        put("alice", "a1", plain);
        put("alice", "a2", plain);
        assert_eq!(color_of("alice", "a1"), random);
        assert_eq!(color_of("alice", "a2"), random);

        // An explicit color is stored (lowercased) and repaints all hosts of the user.
        let with = |color: &str| {
            format!(r#"{{"free_ram_bytes":1,"total_ram_bytes":2,"color":"{color}"}}"#)
        };
        assert_eq!(put("alice", "a1", &with("#AbCdEf")).status(), Status::Ok);
        assert_eq!(color_of("alice", "a1"), "#abcdef");
        assert_eq!(color_of("alice", "a2"), "#abcdef");
        put("alice", "a2", plain);
        assert_eq!(color_of("alice", "a2"), "#abcdef", "kept without color");

        let dash: Value = c.get("/api/v1/dashboard").dispatch().into_json().unwrap();
        assert_eq!(dash["hosts"][0]["color"], "#abcdef");

        for bad in ["red", "#abc", "#abcdeg", "abcdef0", "#abcdef0"] {
            let res = put("bob", "b", &with(bad));
            assert_eq!(res.status(), Status::UnprocessableEntity, "{bad}");
        }
        let res = put(
            "bob",
            "b",
            r#"{"free_ram_bytes":1,"total_ram_bytes":2,"color":null}"#,
        );
        assert_eq!(res.status(), Status::Created, "null means no color");
    }

    #[test]
    fn report_accepts_an_optional_timeout() {
        let c = client();
        let put = |host: &str, extra: &str| {
            c.put(format!("/api/v1/report/a/{host}"))
                .header(ContentType::JSON)
                .header(auth(TOKEN))
                .body(format!(
                    r#"{{"free_ram_bytes":1,"total_ram_bytes":2{extra}}}"#
                ))
                .dispatch()
        };
        for ok in [1, 30, 60] {
            let res = put("h", &format!(r#","timeout_minutes":{ok}"#));
            assert!(res.status().class().is_success(), "{ok}");
            assert_eq!(res.into_json::<Value>().unwrap()["timeout_minutes"], ok);
        }
        for bad in ["0", "61", "-1", "1.5", r#""5""#] {
            let res = put("h", &format!(r#","timeout_minutes":{bad}"#));
            assert_eq!(res.status(), Status::UnprocessableEntity, "{bad}");
        }

        // Without it (or with null), the default applies again.
        let res: Value = put("h", "").into_json().unwrap();
        assert!(res["timeout_minutes"].is_null());
        let res: Value = put("h", r#","timeout_minutes":null"#).into_json().unwrap();
        assert!(res["timeout_minutes"].is_null());
    }

    #[test]
    fn swagger_ui_is_served() {
        let c = client();
        let res = c.get("/api/swagger-ui/index.html").dispatch();
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(res.content_type(), Some(ContentType::HTML));
    }
}
