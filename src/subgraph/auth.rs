use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub issuer: String,
    pub audience: String,
}

impl AuthConfig {
    /// OIDC_PROVIDERS (comma-separated `<issuer>=<audience>`) and OIDC_GROUPS (comma-separated).
    pub fn from_env() -> Result<Self, String> {
        let list = |name: &str| -> Result<Vec<String>, String> {
            let value = std::env::var(name).map_err(|_| format!("{name} is not set"))?;
            Ok(value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect())
        };
        let providers = list("OIDC_PROVIDERS")?
            .iter()
            .map(|p| p.parse())
            .collect::<Result<Vec<_>, _>>()?;
        let groups = list("OIDC_GROUPS")?;
        if providers.is_empty() || groups.is_empty() {
            return Err("OIDC_PROVIDERS and OIDC_GROUPS must not be empty".to_owned());
        }
        Ok(Self { providers, groups })
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    /// `<issuer>=<audience>`, as listed in OIDC_PROVIDERS.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (issuer, audience) = s
            .rsplit_once('=')
            .filter(|(issuer, audience)| !issuer.is_empty() && !audience.is_empty())
            .ok_or_else(|| format!("expected <issuer>=<audience>, got {s:?}"))?;
        Ok(Self {
            issuer: issuer.to_owned(),
            audience: audience.to_owned(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub providers: Vec<Provider>,
    /// The caller must hold one of these groups (short name or SPN).
    pub groups: Vec<String>,
}

#[derive(Deserialize)]
struct Issuer {
    iss: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub groups: Option<Vec<String>>,
}

impl Claims {
    pub fn identity(&self) -> &str {
        self.preferred_username.as_deref().unwrap_or(&self.sub)
    }
}

#[derive(Deserialize)]
struct UserInfo {
    #[serde(default)]
    groups: Vec<String>,
}

/// Kanidm reports groups as SPNs (`name@domain`) or UUIDs; match on either full SPN or name.
fn groups_match(held: &[String], allowed: &[String]) -> bool {
    held.iter().any(|group| {
        let name = group
            .split_once('@')
            .map_or(group.as_str(), |(name, _)| name);
        allowed.iter().any(|a| a == group || a == name)
    })
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
    groups: Vec<String>,
    issuers: Vec<IssuerKeys>,
}

struct IssuerKeys {
    provider: Provider,
    jwks_uri: String,
    userinfo_endpoint: String,
    keys: RwLock<BTreeMap<String, VerifyingKey>>,
}

impl Authenticator {
    const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);

    pub fn new(config: AuthConfig) -> Self {
        let issuers = config
            .providers
            .into_iter()
            .map(|provider| {
                let base = provider.issuer.trim_end_matches('/').to_owned();
                IssuerKeys {
                    jwks_uri: format!("{base}/public_key.jwk"),
                    userinfo_endpoint: format!("{base}/userinfo"),
                    provider,
                    keys: RwLock::new(BTreeMap::new()),
                }
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .user_agent("subgraph")
                    .build()
                    .unwrap_or_default(),
                groups: config.groups,
                issuers,
            }),
        }
    }

    fn issuer_for(&self, token: &str) -> Result<&IssuerKeys, AuthError> {
        let Issuer { iss } = jsonwebtoken::dangerous::insecure_decode_claims(token)
            .map_err(|e| AuthError::Unauthorized(format!("unreadable token claims: {e}")))?;
        self.inner
            .issuers
            .iter()
            .find(|i| i.provider.issuer == iss)
            .ok_or_else(|| AuthError::Unauthorized(format!("untrusted issuer {iss}")))
    }

    /// Verifies the token and its groups; the returned claims always carry the groups.
    pub async fn authorize(&self, token: &str) -> Result<Claims, AuthError> {
        let issuer = self.issuer_for(token)?;
        let mut claims = self.verify(issuer, token).await?;
        if claims.groups.is_none() {
            claims.groups = Some(self.userinfo_groups(issuer, token).await?);
        }

        let allowed = &self.inner.groups;
        if !groups_match(claims.groups.as_deref().unwrap_or_default(), allowed) {
            return Err(AuthError::Forbidden(format!(
                "{} is not in any of {allowed:?}",
                claims.identity()
            )));
        }
        Ok(claims)
    }

    pub async fn authorize_headers(&self, headers: &HeaderMap) -> Result<Claims, AuthError> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer)
            .ok_or_else(|| AuthError::Unauthorized("missing bearer token".to_owned()))?;
        self.authorize(token).await
    }

    async fn userinfo_groups(
        &self,
        issuer: &IssuerKeys,
        token: &str,
    ) -> Result<Vec<String>, AuthError> {
        let info: UserInfo = self
            .inner
            .client
            .get(&issuer.userinfo_endpoint)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(info.groups)
    }

    /// Refreshes every issuer's keys, returning how many were loaded in all. One issuer failing
    /// keeps its previous keys and doesn't stop the rest.
    pub async fn refresh(&self) -> Result<usize, AuthError> {
        let mut total = 0;
        let mut failure = None;
        for issuer in &self.inner.issuers {
            match self.refresh_issuer(issuer).await {
                Ok(count) => total += count,
                Err(e) => {
                    tracing::warn!(issuer = issuer.provider.issuer, "jwks refresh failed: {e}");
                    failure = Some(e);
                }
            }
        }
        match failure {
            Some(e) if total == 0 => Err(e),
            _ => Ok(total),
        }
    }

    async fn refresh_issuer(&self, issuer: &IssuerKeys) -> Result<usize, AuthError> {
        let set: JwkSet = self
            .inner
            .client
            .get(&issuer.jwks_uri)
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
            return Err(AuthError::Jwks(format!(
                "no usable keys at {}",
                issuer.jwks_uri
            )));
        }

        let count = keys.len();
        *issuer.keys.write().await = keys;
        tracing::info!(
            issuer = issuer.provider.issuer,
            keys = count,
            "refreshed jwks"
        );
        Ok(count)
    }

    /// Refreshes keys hourly; unknown key ids also trigger a refresh on demand.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(Self::REFRESH_INTERVAL);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = self.refresh().await {
                tracing::warn!("jwks refresh failed, keeping previous keys: {e}");
            }
        }
    }

    async fn verify(&self, issuer: &IssuerKeys, token: &str) -> Result<Claims, AuthError> {
        let header = decode_header(token)
            .map_err(|e| AuthError::Unauthorized(format!("unreadable token header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| AuthError::Unauthorized("token has no kid".to_owned()))?;

        let mut key = issuer.keys.read().await.get(&kid).cloned();
        if key.is_none() {
            tracing::info!("unknown kid {kid}, refreshing jwks");
            self.refresh_issuer(issuer).await?;
            key = issuer.keys.read().await.get(&kid).cloned();
        }
        let key = key.ok_or_else(|| AuthError::Unauthorized(format!("no key for kid {kid}")))?;

        let mut validation = Validation::new(key.alg.unwrap_or(header.alg));
        validation.set_issuer(&[&issuer.provider.issuer]);
        validation.set_audience(&[&issuer.provider.audience]);

        decode::<Claims>(token, &key.key, &validation)
            .map(|data| data.claims)
            .map_err(|e| AuthError::Unauthorized(format!("token rejected: {e}")))
    }
}

pub fn bearer(value: &str) -> Option<&str> {
    let token = value.trim().strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_match_by_short_name_or_spn() {
        let held = vec![
            "00000000-0000-0000-0000-000000000002".to_owned(),
            "registry_users@idm.example.test".to_owned(),
        ];
        assert!(groups_match(&held, &["registry_users".to_owned()]));
        assert!(groups_match(
            &held,
            &["registry_users@idm.example.test".to_owned()]
        ));
        assert!(!groups_match(&held, &["platform_admins".to_owned()]));
        assert!(!groups_match(&[], &["registry_users".to_owned()]));
    }

    fn authenticator() -> Authenticator {
        Authenticator::new(AuthConfig {
            providers: vec![
                "https://idm.example.test/oauth2/openid/spotify/=spotify"
                    .parse()
                    .unwrap(),
                "https://idm.example.test/oauth2/openid/graph=graph"
                    .parse()
                    .unwrap(),
            ],
            groups: vec!["spotify_users".to_owned()],
        })
    }

    fn token(iss: &str) -> String {
        use base64::Engine;
        let encode = |v: serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        format!(
            "{}.{}.sig",
            encode(serde_json::json!({ "alg": "ES256", "kid": "k" })),
            encode(serde_json::json!({ "iss": iss, "sub": "u" }))
        )
    }

    #[test]
    fn endpoints_derive_from_issuer() {
        let auth = authenticator();
        let spotify = &auth.inner.issuers[0];
        assert_eq!(
            spotify.jwks_uri,
            "https://idm.example.test/oauth2/openid/spotify/public_key.jwk"
        );
        assert_eq!(
            spotify.userinfo_endpoint,
            "https://idm.example.test/oauth2/openid/spotify/userinfo"
        );
    }

    #[tokio::test]
    async fn errors_are_graphql_json() {
        let response = AuthError::Unauthorized("missing bearer token".to_owned()).into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["errors"][0]["message"],
            "unauthorized: missing bearer token"
        );
        assert_eq!(body["errors"][0]["extensions"]["code"], "UNAUTHENTICATED");
    }

    #[test]
    fn providers_parse_issuer_and_audience() {
        assert_eq!(
            "https://idm.example.test/oauth2/openid/graph=graph"
                .parse::<Provider>()
                .unwrap(),
            Provider {
                issuer: "https://idm.example.test/oauth2/openid/graph".to_owned(),
                audience: "graph".to_owned(),
            }
        );
        assert!("https://idm.example.test".parse::<Provider>().is_err());
        assert!("https://idm.example.test=".parse::<Provider>().is_err());
    }

    #[test]
    fn tokens_are_matched_to_their_issuer() {
        let auth = authenticator();
        let graph = auth
            .issuer_for(&token("https://idm.example.test/oauth2/openid/graph"))
            .unwrap();
        assert_eq!(graph.provider.audience, "graph");
        assert!(matches!(
            auth.issuer_for(&token("https://elsewhere.test")),
            Err(AuthError::Unauthorized(_))
        ));
    }
}
