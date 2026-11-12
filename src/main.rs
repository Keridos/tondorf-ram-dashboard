#[macro_use]
extern crate rocket;

mod api;
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            stale_after_secs: 300,
            sweep_interval_secs: 30,
        }
    }
}

impl AppConfig {
    fn from_figment(figment: &Figment) -> Result<(TimeDelta, Duration), String> {
        let cfg: Self = figment.extract().map_err(|e| e.to_string())?;
        let stale_after = i64::try_from(cfg.stale_after_secs)
            .ok()
            .and_then(TimeDelta::try_seconds)
            .filter(|d| *d > TimeDelta::zero())
            .ok_or("stale_after_secs must be between 1 and 9223372036854775")?;
        if cfg.sweep_interval_secs == 0 {
            return Err("sweep_interval_secs must be >= 1".into());
        }
        Ok((stale_after, Duration::from_secs(cfg.sweep_interval_secs)))
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
    // Mounts the routes plus the merged spec at /openapi.json.
    mount_endpoints_and_merged_docs! {
        rocket, "/", settings,
        "" => (vec![], api::spec_info()),
        "/api/v1" => openapi_get_routes_spec![
            settings: api::report_host, api::get_host, api::delete_host, api::dashboard
        ],
        "" => openapi_get_routes_spec![settings: api::health],
    };

    rocket
        .mount(
            "/swagger-ui/",
            make_swagger_ui(&SwaggerUIConfig {
                url: "../openapi.json".to_owned(),
                ..Default::default()
            }),
        )
        .attach(AdHoc::try_on_ignite("Store", |rocket| async move {
            match AppConfig::from_figment(rocket.figment()) {
                Ok((stale_after, sweep_interval)) => {
                    let store = Arc::new(Store::new(stale_after));
                    Ok(rocket.manage(store).manage(SweepInterval(sweep_interval)))
                }
                Err(e) => {
                    eprintln!("invalid configuration: {e}");
                    Err(rocket)
                }
            }
        }))
        .attach(AdHoc::on_liftoff("Stale host sweeper", |rocket| {
            Box::pin(async move {
                let store = Arc::clone(rocket.state::<Arc<Store>>().unwrap());
                let SweepInterval(every) = *rocket.state::<SweepInterval>().unwrap();
                rocket::tokio::spawn(async move {
                    let mut tick = rocket::tokio::time::interval(every);
                    loop {
                        tick.tick().await;
                        store.prune(Utc::now());
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

#[derive(Clone, Copy)]
struct SweepInterval(Duration);

#[launch]
fn rocket() -> _ {
    build()
}

#[cfg(test)]
mod tests {
    use super::{build, build_with};
    use crate::store::Store;
    use rocket::error::ErrorKind;
    use rocket::http::{ContentType, Status};
    use rocket::local::blocking::Client;
    use rocket::serde::json::Value;
    use std::sync::Arc;

    fn client() -> Client {
        Client::tracked(build()).expect("valid rocket instance")
    }

    fn report(c: &Client, host: &str, user: &str, free: u64, total: u64) -> Status {
        c.put(format!("/api/v1/hosts/{host}"))
            .header(ContentType::JSON)
            .body(format!(
                r#"{{"username":"{user}","free_ram_bytes":{free},"total_ram_bytes":{total}}}"#
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
        assert_eq!(report(&c, "h", "", 1, 2), Status::UnprocessableEntity);
        assert_eq!(
            report(&c, "bad%20host", "a", 1, 2),
            Status::UnprocessableEntity
        );

        let res = c
            .put("/api/v1/hosts/h")
            .header(ContentType::JSON)
            .body(r#"{"username":"a"}"#)
            .dispatch();
        assert_eq!(res.status(), Status::UnprocessableEntity);
        let body: Value = res.into_json().unwrap();
        assert!(body["error"].as_str().unwrap().contains("missing field"));
    }

    #[test]
    fn get_and_delete_host() {
        let c = client();
        assert_eq!(
            c.get("/api/v1/hosts/x").dispatch().status(),
            Status::NotFound
        );
        report(&c, "x", "a", 1, 2);
        let host: Value = c.get("/api/v1/hosts/X").dispatch().into_json().unwrap();
        assert_eq!(host["username"], "a");
        assert_eq!(
            c.delete("/api/v1/hosts/x").dispatch().status(),
            Status::NoContent
        );
        assert_eq!(
            c.delete("/api/v1/hosts/x").dispatch().status(),
            Status::NotFound
        );
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
        for (key, val) in [("stale_after_secs", 0), ("sweep_interval_secs", 0)] {
            let figment = rocket::Config::figment().merge((key, val));
            match Client::tracked(build_with(figment)) {
                Ok(_) => panic!("{key}=0 accepted"),
                // Inspecting the error marks it handled (Rocket panics on unhandled drop).
                Err(e) => assert!(matches!(e.kind(), ErrorKind::FailedFairings(_))),
            }
        }
    }

    #[test]
    fn sweeper_removes_stale_hosts() {
        let figment = rocket::Config::figment()
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
            c.get("/api/v1/hosts/gone").dispatch().status(),
            Status::NotFound
        );
    }

    #[test]
    fn openapi_spec_documents_all_routes() {
        let c = client();
        let spec: Value = c.get("/openapi.json").dispatch().into_json().unwrap();
        assert_eq!(spec["info"]["title"], "RAM Dashboard API");

        let codes = |path: &str, method: &str| -> Vec<String> {
            let op = &spec["paths"][path][method];
            assert!(op["summary"].is_string(), "{method} {path} has no summary");
            op["responses"].as_object().unwrap().keys().cloned().collect()
        };
        assert_eq!(codes("/api/v1/hosts/{hostname}", "put"), ["200", "201", "422"]);
        assert_eq!(codes("/api/v1/hosts/{hostname}", "get"), ["200", "404", "422"]);
        assert_eq!(codes("/api/v1/hosts/{hostname}", "delete"), ["204", "404", "422"]);
        assert_eq!(codes("/api/v1/dashboard", "get"), ["200", "422"]);
        assert_eq!(codes("/health", "get"), ["200"]);

        let schemas = &spec["components"]["schemas"];
        for name in ["HostReport", "Host", "RankedHost", "Dashboard", "ErrorBody"] {
            assert!(schemas[name].is_object(), "schema {name} missing");
        }
    }

    #[test]
    fn swagger_ui_is_served() {
        let c = client();
        let res = c.get("/swagger-ui/index.html").dispatch();
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(res.content_type(), Some(ContentType::HTML));
    }
}
