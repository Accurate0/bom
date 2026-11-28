use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::subgraph::external::External;
use crate::subgraph::passport::{self, IdentityKind, Passport};

pub type Grants = BTreeMap<String, BTreeMap<String, Vec<serde_json::Value>>>;

/// Kanidm sends groups as SPNs (`name@domain`) and uuids; either the SPN or its short name matches.
fn groups_match(held: &[String], allowed: &[String]) -> bool {
    held.iter().any(|group| {
        let name = group
            .split_once('@')
            .map_or(group.as_str(), |(name, _)| name);
        allowed.iter().any(|a| a == group || a == name)
    })
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("jwks error: {0}")]
    Jwks(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

/// GraphQL-shaped so the router passes the reason on instead of rejecting a non-JSON body.
impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, "FORBIDDEN"),
            Self::Jwks(_) | Self::Http(_) => (StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE"),
        };
        let body = serde_json::json!({
            "errors": [{ "message": self.to_string(), "extensions": { "code": code } }]
        });
        (status, axum::Json(body)).into_response()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub audience: String,
    pub jwks: String,
    #[serde(default)]
    pub grants: Grants,
}

impl AuthConfig {
    pub fn from_env() -> Result<Self, String> {
        let json = std::env::var("SUBGRAPH_AUTH").map_err(|_| "SUBGRAPH_AUTH is not set")?;
        Self::from_subgraph_auth(&json)
    }

    pub fn from_subgraph_auth(json: &str) -> Result<Self, String> {
        let config: Self =
            serde_json::from_str(json).map_err(|e| format!("SUBGRAPH_AUTH is not valid: {e}"))?;
        if config.audience.is_empty() || config.jwks.is_empty() {
            return Err("SUBGRAPH_AUTH needs an audience and the router's jwks".to_owned());
        }
        Ok(config)
    }
}

#[derive(Debug, Clone)]
pub enum Principal {
    User(Claims),
    External(External),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub jti: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub groups: Option<Vec<String>>,
    /// Space-separated, as Kanidm issues it.
    #[serde(default)]
    pub scope: Option<String>,
}

impl Claims {
    pub fn identity(&self) -> &str {
        self.preferred_username.as_deref().unwrap_or(&self.sub)
    }

    pub fn scopes(&self) -> BTreeSet<&str> {
        self.scope
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .collect()
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes().contains(scope)
    }

    pub fn in_any(&self, allowed: &[&str]) -> bool {
        let allowed: Vec<String> = allowed.iter().map(|a| (*a).to_owned()).collect();
        groups_match(self.groups.as_deref().unwrap_or_default(), &allowed)
    }
}

impl Principal {
    pub fn scopes(&self) -> BTreeSet<String> {
        match self {
            Principal::User(claims) => claims.scopes().into_iter().map(str::to_owned).collect(),
            Principal::External(external) => external.scopes.clone(),
        }
    }
}

impl From<Passport> for Principal {
    fn from(passport: Passport) -> Self {
        let identity = passport.identity;
        match identity.kind {
            IdentityKind::User => Principal::User(Claims {
                sub: identity.subject,
                jti: None,
                preferred_username: identity.username,
                groups: Some(identity.groups),
                scope: Some(identity.scopes.into_iter().collect::<Vec<_>>().join(" ")),
            }),
            IdentityKind::External => Principal::External(External {
                issuer: identity.issuer,
                claims: identity.claims,
                scopes: identity.scopes,
            }),
        }
    }
}

#[derive(Clone)]
struct VerifyingKey {
    key: DecodingKey,
    alg: Option<Algorithm>,
}

#[derive(Clone)]
pub struct Authenticator {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    config: AuthConfig,
    keys: RwLock<BTreeMap<String, VerifyingKey>>,
    last_refresh: Mutex<Option<Instant>>,
}

impl Authenticator {
    const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
    const UNKNOWN_KID_COOLDOWN: Duration = Duration::from_secs(30);

    pub fn new(config: AuthConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .user_agent("subgraph")
                    .build()
                    .unwrap_or_default(),
                config,
                keys: RwLock::new(BTreeMap::new()),
                last_refresh: Mutex::new(None),
            }),
        }
    }

    pub fn config(&self) -> &AuthConfig {
        &self.inner.config
    }

    pub async fn authenticate(&self, passport: &str) -> Result<Principal, AuthError> {
        self.verify(passport).await.map(Principal::from)
    }

    pub async fn authenticate_headers(&self, headers: &HeaderMap) -> Result<Principal, AuthError> {
        let passport = headers
            .get(passport::HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| AuthError::Unauthorized("missing graph passport".to_owned()))?;
        self.authenticate(passport).await
    }

    pub async fn refresh(&self) -> Result<usize, AuthError> {
        let mut last = self.inner.last_refresh.lock().await;
        *last = Some(Instant::now());
        self.fetch_keys().await
    }

    // an in-flight refresh may bring the kid; otherwise refresh at most once per cooldown
    async fn refresh_for(&self, kid: &str) -> Result<(), AuthError> {
        let mut last = self.inner.last_refresh.lock().await;
        let known = self.inner.keys.read().await.contains_key(kid);
        if known || last.is_some_and(|at| at.elapsed() < Self::UNKNOWN_KID_COOLDOWN) {
            return Ok(());
        }

        tracing::info!("unknown passport kid {kid}, refreshing jwks");
        *last = Some(Instant::now());
        self.fetch_keys().await.map(|_| ())
    }

    async fn fetch_keys(&self) -> Result<usize, AuthError> {
        let jwks = &self.inner.config.jwks;

        let set: JwkSet = self
            .inner
            .client
            .get(jwks)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut keys = BTreeMap::new();
        for jwk in &set.keys {
            let Some(kid) = jwk.common.key_id.clone() else {
                continue;
            };
            let asymmetric = matches!(
                jwk.algorithm,
                AlgorithmParameters::RSA(_)
                    | AlgorithmParameters::EllipticCurve(_)
                    | AlgorithmParameters::OctetKeyPair(_)
            );
            if !asymmetric {
                tracing::warn!("ignoring non-asymmetric jwk {kid}");
                continue;
            }
            match DecodingKey::from_jwk(jwk) {
                Ok(key) => {
                    let alg = jwk
                        .common
                        .key_algorithm
                        .and_then(|a| a.to_string().parse::<Algorithm>().ok());
                    keys.insert(kid, VerifyingKey { key, alg });
                }
                Err(e) => tracing::warn!("ignoring unusable jwk {kid}: {e}"),
            }
        }

        if keys.is_empty() {
            return Err(AuthError::Jwks(format!("no usable keys at {jwks}")));
        }

        let count = keys.len();
        *self.inner.keys.write().await = keys;
        tracing::info!(jwks, keys = count, "refreshed passport jwks");
        Ok(count)
    }

    /// Refreshes keys hourly; unknown key ids also trigger a refresh on demand.
    pub async fn run(self) {
        if let Err(e) = self.refresh().await {
            tracing::warn!("initial passport jwks fetch failed, retrying on demand: {e}");
        }
        let mut ticker = tokio::time::interval(Self::REFRESH_INTERVAL);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = self.refresh().await {
                tracing::warn!("passport jwks refresh failed, keeping previous keys: {e}");
            }
        }
    }

    async fn verify(&self, token: &str) -> Result<Passport, AuthError> {
        let header = decode_header(token)
            .map_err(|e| AuthError::Unauthorized(format!("unreadable passport header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| AuthError::Unauthorized("passport has no kid".to_owned()))?;

        let mut key = self.inner.keys.read().await.get(&kid).cloned();
        if key.is_none() {
            self.refresh_for(&kid).await?;
            key = self.inner.keys.read().await.get(&kid).cloned();
        }
        let key = key.ok_or_else(|| AuthError::Unauthorized(format!("no key for kid {kid}")))?;

        let mut validation = Validation::new(key.alg.unwrap_or(header.alg));
        validation.set_issuer(&[passport::ISSUER]);
        validation.set_audience(&[&self.inner.config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = 5;

        decode::<Passport>(token, &key.key, &validation)
            .map(|data| data.claims)
            .map_err(|e| AuthError::Unauthorized(format!("passport rejected: {e}")))
    }
}

pub fn bearer(value: &str) -> Option<&str> {
    let token = value.trim().strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};

    // a throwaway P-256 key, only for signing test passports
    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQrxIw5f6gChTjB4J
goqO/XL/jTOI1DURt4md/xUx3qChRANCAARRtn8X4BTpCgfXkHeNNosv447OcYmC
WTbO2iQACXuyX0kacNHv2hA0Kb33waLg/85mkeG1cQVfOEMQq8jA8xNE
-----END PRIVATE KEY-----";
    const TEST_X: &str = "UbZ_F-AU6QoH15B3jTaLL-OOznGJglk2ztokAAl7sl8";
    const TEST_Y: &str = "SRpw0e_aEDQpvffBouD_zmaR4bVxBV84QxCryMDzE0Q";

    async fn authenticator() -> Authenticator {
        let auth = Authenticator::new(AuthConfig {
            audience: "bom".to_owned(),
            jwks: "http://router.invalid/.well-known/passport.jwks".to_owned(),
            grants: Grants::default(),
        });
        let set: JwkSet = serde_json::from_value(serde_json::json!({ "keys": [{
            "kty": "EC", "crv": "P-256", "x": TEST_X, "y": TEST_Y, "kid": "router", "alg": "ES256",
        }] }))
        .unwrap();
        auth.inner.keys.write().await.insert(
            "router".to_owned(),
            VerifyingKey {
                key: DecodingKey::from_jwk(&set.keys[0]).unwrap(),
                alg: Some(Algorithm::ES256),
            },
        );
        *auth.inner.last_refresh.lock().await = Some(Instant::now());
        auth
    }

    fn passport(overrides: serde_json::Value) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut claims = serde_json::json!({
            "iss": passport::ISSUER,
            "aud": "bom",
            "sub": "uuid",
            "iat": now,
            "exp": now + 60,
            "identity": {
                "issuer": "kanidm",
                "kind": "user",
                "subject": "uuid",
                "username": "anurag",
                "scopes": ["bom:radar:read"],
                "groups": ["bom_users@idm.example.test"],
            },
        });
        for (key, value) in overrides.as_object().unwrap() {
            claims[key] = value.clone();
        }

        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("router".to_owned());
        let key = EncodingKey::from_ec_pem(TEST_KEY.as_bytes()).unwrap();
        encode(&header, &claims, &key).unwrap()
    }

    #[tokio::test]
    async fn user_passports_become_claims() {
        let auth = authenticator().await;
        let Principal::User(claims) = auth
            .authenticate(&passport(serde_json::json!({})))
            .await
            .unwrap()
        else {
            panic!("expected a user");
        };
        assert_eq!(claims.identity(), "anurag");
        assert!(claims.has_scope("bom:radar:read"));
        assert!(claims.in_any(&["bom_users"]));
    }

    #[tokio::test]
    async fn passports_for_others_or_from_elsewhere_are_rejected() {
        let auth = authenticator().await;
        for overrides in [
            serde_json::json!({ "aud": "registry" }),
            serde_json::json!({ "iss": "https://idm.anurag.sh/oauth2/openid/graph" }),
            serde_json::json!({ "exp": 1 }),
        ] {
            assert!(matches!(
                auth.authenticate(&passport(overrides)).await,
                Err(AuthError::Unauthorized(_))
            ));
        }
    }

    #[tokio::test]
    async fn requests_without_a_passport_are_unauthorized() {
        let auth = authenticator().await;
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer kanidm-token".parse().unwrap());
        assert!(matches!(
            auth.authenticate_headers(&headers).await,
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[test]
    fn subgraph_auth_needs_an_audience_and_jwks() {
        let config = AuthConfig::from_subgraph_auth(
            r#"{"audience":"bom","jwks":"http://router/.well-known/passport.jwks","grants":{}}"#,
        )
        .unwrap();
        assert_eq!(config.audience, "bom");
        assert!(AuthConfig::from_subgraph_auth(r#"{"audience":"","jwks":"x"}"#).is_err());
        assert!(AuthConfig::from_subgraph_auth(r#"{"providers":[]}"#).is_err());
    }

    #[test]
    fn groups_match_by_short_name_or_spn() {
        let held = vec!["bom_users@idm.example.test".to_owned()];
        assert!(groups_match(&held, &["bom_users".to_owned()]));
        assert!(!groups_match(&held, &["platform_admins".to_owned()]));
    }
}
