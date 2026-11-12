//! HTTP routes and their OpenAPI documentation.
//!
//! Doc comments on route functions become the spec: a leading `# Title` line is
//! the operation summary, the rest is the description.

use std::sync::Arc;

use chrono::Utc;
use rocket::http::Status;
use rocket::response::{self, Responder, status::NoContent};
use rocket::serde::json::{self, Json};
use rocket::{Request, State};
use rocket_okapi::r#gen::OpenApiGenerator;
use rocket_okapi::okapi::openapi3::{Info, OpenApi, RefOr, Responses};
use rocket_okapi::openapi;
use rocket_okapi::response::OpenApiResponderInner;
use rocket_okapi::util::add_schema_response;
use schemars::JsonSchema;
use serde::Serialize;

use crate::model::{Dashboard, ErrorBody, Host, HostReport, normalize_hostname};
use crate::store::Store;

// ---------------------------------------------------------------------------
// Responders
// ---------------------------------------------------------------------------

// Errors are always serialized as `{"error": "..."}`. There are two types so
// each route's spec lists only the error codes it can actually return.

/// `422 Unprocessable Entity`: the request was understood but is invalid.
#[derive(Debug)]
pub struct Invalid(pub String);

/// `404 Not Found` or `422 Unprocessable Entity`.
#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    Invalid(String),
}

impl From<Invalid> for ApiError {
    fn from(Invalid(e): Invalid) -> Self {
        ApiError::Invalid(e)
    }
}

fn error_response(status: Status, error: String, req: &Request<'_>) -> response::Result<'static> {
    (status, Json(ErrorBody { error })).respond_to(req)
}

impl<'r> Responder<'r, 'static> for Invalid {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        error_response(Status::UnprocessableEntity, self.0, req)
    }
}

impl<'r> Responder<'r, 'static> for ApiError {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        match self {
            ApiError::NotFound(e) => error_response(Status::NotFound, e, req),
            ApiError::Invalid(e) => error_response(Status::UnprocessableEntity, e, req),
        }
    }
}

const INVALID_DESC: &str = "Invalid input: malformed JSON, bad hostname, free > total, ...";

impl OpenApiResponderInner for Invalid {
    fn responses(r#gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let mut responses = Responses::default();
        add_schema_response(
            &mut responses,
            422,
            "application/json",
            r#gen.json_schema::<ErrorBody>(),
        )?;
        describe(&mut responses, 422, INVALID_DESC);
        Ok(responses)
    }
}

impl OpenApiResponderInner for ApiError {
    fn responses(r#gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let mut responses = Invalid::responses(r#gen)?;
        add_schema_response(
            &mut responses,
            404,
            "application/json",
            r#gen.json_schema::<ErrorBody>(),
        )?;
        describe(&mut responses, 404, "Host not found (unknown or stale)");
        Ok(responses)
    }
}

/// Successful report: `201` if the host is new (or was stale), `200` otherwise.
pub struct Reported {
    created: bool,
    host: Host,
}

impl<'r> Responder<'r, 'static> for Reported {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        let status = if self.created {
            Status::Created
        } else {
            Status::Ok
        };
        (status, Json(self.host)).respond_to(req)
    }
}

impl OpenApiResponderInner for Reported {
    fn responses(r#gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let schema = r#gen.json_schema::<Host>();
        let mut responses = Responses::default();
        add_schema_response(&mut responses, 200, "application/json", schema.clone())?;
        add_schema_response(&mut responses, 201, "application/json", schema)?;
        describe(&mut responses, 200, "Existing host updated");
        describe(
            &mut responses,
            201,
            "New host registered (or a stale host re-appeared)",
        );
        Ok(responses)
    }
}

fn describe(responses: &mut Responses, status: u16, description: &str) {
    if let Some(RefOr::Object(r)) = responses.responses.get_mut(&status.to_string()) {
        r.description = description.to_owned();
    }
}

fn parse_hostname(raw: &str) -> Result<String, Invalid> {
    normalize_hostname(raw).map_err(Invalid)
}

fn not_found(hostname: &str) -> ApiError {
    ApiError::NotFound(format!("host '{hostname}' not found"))
}

// ---------------------------------------------------------------------------
// Routes (mounted under /api/v1)
// ---------------------------------------------------------------------------

/// # Report the current memory state of a host
///
/// Idempotent upsert, called periodically by an agent on each host. Replaces
/// the previous report for the hostname and sets `last_seen` to server time.
/// Hostnames are 1-253 chars of `[A-Za-z0-9.-_]`, case-insensitive.
#[openapi(tag = "Reporting")]
#[put("/hosts/<hostname>", format = "json", data = "<report>")]
pub fn report_host(
    hostname: &str,
    report: Result<Json<HostReport>, json::Error<'_>>,
    store: &State<Arc<Store>>,
) -> Result<Reported, Invalid> {
    let hostname = parse_hostname(hostname)?;
    let report = report
        .map_err(|e| Invalid(format!("invalid body: {e}")))?
        .into_inner();
    report.validate().map_err(Invalid)?;

    let host = Host {
        hostname,
        username: report.username.trim().to_owned(),
        free_ram_bytes: report.free_ram_bytes,
        total_ram_bytes: report.total_ram_bytes,
        last_seen: Utc::now(),
    };
    let created = store.upsert(host.clone());
    Ok(Reported { created, host })
}

/// # Get the latest report of a single host
#[openapi(tag = "Dashboard")]
#[get("/hosts/<hostname>")]
pub fn get_host(hostname: &str, store: &State<Arc<Store>>) -> Result<Json<Host>, ApiError> {
    let hostname = parse_hostname(hostname)?;
    store
        .get(&hostname, Utc::now())
        .map(Json)
        .ok_or_else(|| not_found(&hostname))
}

/// # Remove a host from the dashboard
#[openapi(tag = "Reporting")]
#[delete("/hosts/<hostname>")]
pub fn delete_host(hostname: &str, store: &State<Arc<Store>>) -> Result<NoContent, ApiError> {
    let hostname = parse_hostname(hostname)?;
    if store.remove(&hostname, Utc::now()) {
        Ok(NoContent)
    } else {
        Err(not_found(&hostname))
    }
}

/// # Get all hosts ranked by free RAM
///
/// Sorted by `free_ram_bytes` descending, ties broken by hostname ascending;
/// `rank` starts at 1. Stale hosts are excluded. Use `limit` (>= 1) to get
/// only the top N hosts.
#[openapi(tag = "Dashboard")]
#[get("/dashboard?<limit>")]
pub fn dashboard(
    limit: Option<usize>,
    store: &State<Arc<Store>>,
) -> Result<Json<Dashboard>, Invalid> {
    if limit == Some(0) {
        return Err(Invalid("limit must be >= 1".into()));
    }
    Ok(Json(store.dashboard(limit, Utc::now())))
}

// ---------------------------------------------------------------------------
// Routes (mounted under /)
// ---------------------------------------------------------------------------

#[derive(Serialize, JsonSchema)]
pub struct Health {
    status: &'static str,
}

/// # Liveness probe
#[openapi(tag = "Meta")]
#[get("/health")]
pub fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

/// Top-level spec metadata; merged with the specs generated from the routes.
pub fn spec_info() -> OpenApi {
    OpenApi {
        openapi: OpenApi::default_version(),
        info: Info {
            title: "RAM Dashboard API".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            description: Some(
                "Hosts report their memory usage; the service keeps the latest \
                 report per host and exposes a dashboard ranked by free RAM.\n\n\
                 - All memory values are **bytes** (u64).\n\
                 - Timestamps are RFC 3339 in UTC.\n\
                 - Hostnames are case-insensitive and stored lowercased.\n\
                 - Hosts that haven't reported for `stale_after_secs` (default 300, \
                 `ROCKET_STALE_AFTER_SECS`) are removed automatically."
                    .to_owned(),
            ),
            ..Default::default()
        },
        ..Default::default()
    }
}
