//! Authentication of every API call (decision D7: loopback included).

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use subtle::ConstantTimeEq;

use crate::config::{ApiConfig, ConfigError};

/// Longest `Authorization` header looked at.
const MAX_AUTHORIZATION_LEN: usize = 1024;

/// Who may call the API.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    /// HTTP Basic, a bearer token, or both (either one is enough).
    Credentials {
        /// User name and password for HTTP Basic.
        basic: Option<(String, String)>,
        /// Token for `Authorization: Bearer`.
        bearer: Option<String>,
    },
    /// Anyone. Only by explicit configuration.
    Unauthenticated,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Credentials { basic, bearer } => f
                .debug_struct("Credentials")
                .field("basic", &basic.as_ref().map(|(user, _)| (user.as_str(), "***")))
                .field("bearer", &bearer.as_ref().map(|_| "***"))
                .finish(),
            Self::Unauthenticated => f.write_str("Unauthenticated"),
        }
    }
}

impl Auth {
    /// Reads `api.username`/`api.password`/`api.token`. Refuses a half
    /// credential (a user name without its password, or the reverse), and an
    /// API with no credential at all unless `api.allow_unauthenticated` is set.
    pub fn from_config(api: &ApiConfig) -> Result<Self, ConfigError> {
        let basic = match (&api.username, &api.password) {
            (Some(user), Some(pass)) => Some((user.clone(), pass.clone())),
            (None, None) => None,
            (Some(_), None) => return Err(ConfigError::Invalid("api.username is set without api.password".into())),
            (None, Some(_)) => return Err(ConfigError::Invalid("api.password is set without api.username".into())),
        };
        let bearer = api.token.clone();
        if basic.is_none() && bearer.is_none() {
            return if api.allow_unauthenticated { Ok(Self::Unauthenticated) } else { Err(ConfigError::NoAuth) };
        }
        Ok(Self::Credentials { basic, bearer })
    }

    /// Whether the request's `Authorization` header is acceptable.
    pub fn allows(&self, headers: &HeaderMap) -> bool {
        let (basic, bearer) = match self {
            Self::Unauthenticated => return true,
            Self::Credentials { basic, bearer } => (basic, bearer),
        };
        let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else { return false };
        if value.len() > MAX_AUTHORIZATION_LEN {
            return false;
        }
        let Some((scheme, credential)) = value.trim().split_once(' ') else { return false };
        let credential = credential.trim();
        if scheme.eq_ignore_ascii_case("basic") {
            let Some((user, pass)) = basic else { return false };
            let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(credential) else { return false };
            let Some(colon) = decoded.iter().position(|&b| b == b':') else { return false };
            let (given_user, given_pass) = decoded.split_at(colon);
            let given_pass = given_pass.get(1..).unwrap_or_default();
            let ok = given_user.ct_eq(user.as_bytes()) & given_pass.ct_eq(pass.as_bytes());
            bool::from(ok)
        } else if scheme.eq_ignore_ascii_case("bearer") {
            let Some(token) = bearer else { return false };
            bool::from(credential.as_bytes().ct_eq(token.as_bytes()))
        } else {
            false
        }
    }
}

/// The middleware: `401` with `WWW-Authenticate: Basic` unless allowed.
pub(crate) async fn require(State(auth): State<std::sync::Arc<Auth>>, request: Request, next: Next) -> Response {
    if auth.allows(request.headers()) {
        return next.run(request).await;
    }
    tracing::debug!(path = crate::resource::loggable(request.uri().path()), "api: unauthorized request");
    let mut response = (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"oxrtc\""));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(user: Option<&str>, pass: Option<&str>, token: Option<&str>, open: bool) -> ApiConfig {
        ApiConfig {
            username: user.map(Into::into),
            password: pass.map(Into::into),
            token: token.map(Into::into),
            allow_unauthenticated: open,
            ..ApiConfig::default()
        }
    }

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
    }

    #[test]
    fn from_config_refuses_nothing_and_half_credentials() {
        assert!(matches!(Auth::from_config(&api(None, None, None, false)), Err(ConfigError::NoAuth)));
        assert_eq!(Auth::from_config(&api(None, None, None, true)).unwrap(), Auth::Unauthenticated);
        assert!(matches!(Auth::from_config(&api(Some("u"), None, None, false)), Err(ConfigError::Invalid(_))));
        assert!(matches!(Auth::from_config(&api(None, Some("p"), None, false)), Err(ConfigError::Invalid(_))));
        // Credentials win over the opt-in.
        let auth = Auth::from_config(&api(Some("u"), Some("p"), None, true)).unwrap();
        assert!(matches!(auth, Auth::Credentials { basic: Some(_), bearer: None }));
    }

    #[test]
    fn checks_basic_and_bearer() {
        let auth = Auth::from_config(&api(Some("admin"), Some("p:ss"), Some("tok"), false)).unwrap();
        assert!(auth.allows(&headers(&basic("admin", "p:ss"))));
        assert!(auth.allows(&headers("Bearer tok")));
        assert!(auth.allows(&headers("bearer  tok ")));
        assert!(!auth.allows(&headers(&basic("admin", "p:s"))));
        assert!(!auth.allows(&headers(&basic("admi", "p:ss"))));
        assert!(!auth.allows(&headers("Bearer to")));
        assert!(!auth.allows(&headers("Basic !!!notbase64")));
        assert!(!auth.allows(&headers("Digest x")));
        assert!(!auth.allows(&HeaderMap::new()));
        assert!(!auth.allows(&headers(&format!("Bearer {}", "t".repeat(2000)))));
        let only_basic = Auth::from_config(&api(Some("a"), Some("s3cret"), None, false)).unwrap();
        assert!(!only_basic.allows(&headers("Bearer ")));
        assert!(!format!("{only_basic:?} {auth:?}").contains("s3cret"));
        assert!(!format!("{auth:?}").contains("tok\""));
    }
}
