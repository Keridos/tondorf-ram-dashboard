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
use rocket::http::{Header, Status};
use rocket::serde::json::Json;
use rocket::{Build, Request, Rocket};
use rocket_okapi::settings::OpenApiSettings;
use rocket_okapi::swagger_ui::{SwaggerUIConfig, make_swagger_ui};
use rocket_okapi::{mount_endpoints_and_merged_docs, openapi_get_routes_spec};
use serde::Deserialize;

use auth::ApiToken;
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
    /// Shared bearer token required for PUT and DELETE.
    api_token: String,
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
            api_token: "tondorf.is.cool".into(),
            database_path: "wuppp.db".into(),
            history_retention_days: 30,
        }
    }
}

/// Validated settings.
struct Settings {
    stale_after: TimeDelta,
    sweep: Sweep,
    api_token: String,
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
        if cfg.api_token.trim().is_empty() {
            return Err("api_token must not be empty".into());
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
            api_token: cfg.api_token,
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
            api::dashboard
        ],
        "" => openapi_get_routes_spec![settings: api::health],
    };

    rocket
        // The dashboard page at /api/ (outside the spec).
        .mount(format!("{}/", api::BASE_PATH), routes![api::frontend])
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
            Ok(rocket
                .manage(Arc::new(Store::new(settings.stale_after)))
                .manage(history)
                .manage(ApiToken(settings.api_token))
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
    use rocket::serde::json::Value;
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
    fn token_is_configurable() {
        let c = Client::tracked(build_with(figment().merge(("api_token", "other")))).unwrap();
        assert_eq!(report(&c, "h", "a", 1, 2), Status::Unauthorized);
        let status = c
            .delete("/api/v1/report/a/h")
            .header(auth("other"))
            .dispatch()
            .status();
        assert_eq!(status, Status::NotFound);
    }

    #[test]
    fn history_records_every_report() {
        let c = client();
        for free in [10, 20, 30] {
            report(&c, "H", "a", free, 100);
        }
        report(&c, "other", "a", 1, 100);
        c.delete("/api/v1/report/a/h")
            .header(auth(TOKEN))
            .dispatch();

        let get = |q: &str| c.get(format!("/api/v1/report/a/h/history{q}")).dispatch();
        let body: Value = get("").into_json().unwrap();
        assert_eq!(body["hostname"], "h");
        let frees: Vec<_> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["free_ram_bytes"].as_u64().unwrap())
            .collect();
        assert_eq!(frees, [30, 20, 10], "newest first, kept after DELETE");

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
    fn unknown_route_returns_json_404() {
        let c = client();
        let res = c.get("/nope").dispatch();
        assert_eq!(res.status(), Status::NotFound);
        assert_eq!(res.content_type(), Some(ContentType::JSON));
    }

    #[test]
    fn invalid_stale_config_fails_to_start() {
        let bad: [(&str, Value); 4] = [
            ("stale_after_secs", 0.into()),
            ("sweep_interval_secs", 0.into()),
            ("api_token", " ".into()),
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
        assert_eq!(codes(host, "delete"), ["204", "401", "404", "422"]);
        assert_eq!(
            codes("/v1/report/{username}/{hostname}/history", "get"),
            ["200", "422", "500"]
        );
        assert_eq!(codes("/v1/dashboard", "get"), ["200", "422"]);
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
        for path in ["/api/", "/api"] {
            let res = c.get(path).dispatch();
            assert_eq!(res.status(), Status::Ok, "{path}");
            assert_eq!(res.content_type(), Some(ContentType::HTML));
            assert!(res.into_string().unwrap().contains("v1/dashboard"));
        }
        assert_eq!(c.get("/").dispatch().status(), Status::NotFound);
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
    fn swagger_ui_is_served() {
        let c = client();
        let res = c.get("/api/swagger-ui/index.html").dispatch();
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(res.content_type(), Some(ContentType::HTML));
    }
}
