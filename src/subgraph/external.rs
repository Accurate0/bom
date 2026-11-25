use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wildmatch::WildMatch;

/// A non-Kanidm OIDC issuer whose tokens identify a machine by its claims, not by groups.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExternalIssuer {
    pub name: String,
    pub issuer: String,
    pub jwks: String,
    pub audience: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn globs(&self) -> &[String] {
        match self {
            Self::One(glob) => std::slice::from_ref(glob),
            Self::Many(globs) => globs,
        }
    }
}

/// Admits an external token when every listed claim is a string matching one of its globs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClaimPattern {
    pub issuer: String,
    pub claims: BTreeMap<String, OneOrMany>,
}

impl ClaimPattern {
    pub fn matches(&self, issuer: &str, claims: &Value) -> bool {
        if self.issuer != issuer {
            return false;
        }

        self.claims.iter().all(|(name, allowed)| {
            let Some(value) = claims.get(name).and_then(Value::as_str) else {
                return false;
            };

            let value = value.to_lowercase();
            allowed
                .globs()
                .iter()
                .any(|glob| WildMatch::new(&glob.to_lowercase()).matches(&value))
        })
    }
}

pub fn any_matches(patterns: &[ClaimPattern], issuer: &str, claims: &Value) -> bool {
    patterns.iter().any(|p| p.matches(issuer, claims))
}

/// A verified external token that matched one of the subgraph's claim patterns.
#[derive(Debug, Clone)]
pub struct External {
    /// The issuer's name from `ExternalIssuer`, e.g. `forgejo`.
    pub issuer: String,
    pub claims: Value,
    /// Scopes granted by the claim patterns this token matched.
    pub scopes: std::collections::BTreeSet<String>,
}

impl External {
    pub fn claims_as<T: serde::de::DeserializeOwned>(&self) -> async_graphql::Result<T> {
        serde_json::from_value(self.claims.clone()).map_err(|e| {
            async_graphql::Error::new(format!("unexpected {} claims: {e}", self.issuer))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pattern(claims: Value) -> ClaimPattern {
        serde_json::from_value(json!({ "issuer": "forgejo", "claims": claims })).unwrap()
    }

    #[test]
    fn every_listed_claim_must_match_one_glob() {
        let claims = json!({
            "repository": "Anurag/k8s",
            "ref": "refs/heads/main",
            "event_name": "push",
        });

        assert!(pattern(json!({ "repository": "*" })).matches("forgejo", &claims));
        assert!(
            pattern(json!({ "repository": "anurag/k8s", "ref": "refs/heads/main" }))
                .matches("forgejo", &claims)
        );
        assert!(
            pattern(json!({ "event_name": ["push", "workflow_dispatch"] }))
                .matches("forgejo", &claims)
        );
        assert!(!pattern(json!({ "repository": "anurag/spotify" })).matches("forgejo", &claims));
        assert!(!pattern(json!({ "workflow_ref": "*" })).matches("forgejo", &claims));
        assert!(!pattern(json!({ "repository": "*" })).matches("github", &claims));
    }

    #[test]
    fn pull_requests_can_be_kept_out() {
        let pr = json!({ "repository": "anurag/k8s", "event_name": "pull_request" });
        let only_pushes =
            pattern(json!({ "event_name": ["push", "workflow_dispatch", "schedule"] }));
        assert!(!only_pushes.matches("forgejo", &pr));
    }
}
