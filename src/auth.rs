//! Shared-token authentication for write routes.
//!
//! Clients send `Authorization: Bearer <token>`, where the token is any one of
//! `api_tokens` in Rocket.toml (or `ROCKET_API_TOKENS='["a","b"]'`).

use rocket::Request;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket_okapi::r#gen::OpenApiGenerator;
use rocket_okapi::okapi::Map;
use rocket_okapi::okapi::openapi3::{
    Object, RefOr, Responses, SecurityRequirement, SecurityScheme, SecuritySchemeData,
};
use rocket_okapi::request::{OpenApiFromRequest, RequestHeaderInput};
use rocket_okapi::util::add_schema_response;

use crate::model::ErrorBody;

/// The configured tokens, held in managed state.
pub struct ApiTokens(pub Vec<String>);

/// Request guard: succeeds only if the request carries the correct bearer token.
/// On failure the request is forwarded to the JSON catcher as `401`.
pub struct WriteAuth;

const SCHEME_NAME: &str = "bearerAuth";

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WriteAuth {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, ()> {
        let accepted = &req
            .rocket()
            .state::<ApiTokens>()
            .expect("ApiTokens is managed")
            .0;
        let provided = req
            .headers()
            .get_one("Authorization")
            .and_then(|v| v.strip_prefix("Bearer "));
        match provided {
            Some(token) if matches_any(token, accepted) => Outcome::Success(WriteAuth),
            _ => Outcome::Error((Status::Unauthorized, ())),
        }
    }
}

/// Checks every token (no early exit), so timing doesn't reveal which one matched.
fn matches_any(provided: &str, accepted: &[String]) -> bool {
    accepted.iter().fold(false, |found, t| {
        found | constant_time_eq(provided.as_bytes(), t.as_bytes())
    })
}

/// Compares without short-circuiting on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl<'r> OpenApiFromRequest<'r> for WriteAuth {
    fn from_request_input(
        _gen: &mut OpenApiGenerator,
        _name: String,
        _required: bool,
    ) -> rocket_okapi::Result<RequestHeaderInput> {
        let scheme = SecurityScheme {
            description: Some("Shared API token: `Authorization: Bearer <token>`".to_owned()),
            data: SecuritySchemeData::Http {
                scheme: "bearer".to_owned(),
                bearer_format: None,
            },
            extensions: Object::default(),
        };
        let mut requirement: SecurityRequirement = Map::new();
        requirement.insert(SCHEME_NAME.to_owned(), Vec::new());
        Ok(RequestHeaderInput::Security(
            SCHEME_NAME.to_owned(),
            scheme,
            requirement,
        ))
    }

    fn get_responses(r#gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let mut responses = Responses::default();
        add_schema_response(
            &mut responses,
            401,
            "application/json",
            r#gen.json_schema::<ErrorBody>(),
        )?;
        if let Some(RefOr::Object(r)) = responses.responses.get_mut("401") {
            r.description = "Missing or wrong API token".to_owned();
        }
        Ok(responses)
    }
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, matches_any};

    #[test]
    fn token_comparison() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn any_configured_token_matches() {
        let accepted = ["a".to_owned(), "bb".to_owned()];
        assert!(matches_any("a", &accepted));
        assert!(matches_any("bb", &accepted));
        assert!(!matches_any("b", &accepted));
        assert!(!matches_any("", &accepted));
    }
}
