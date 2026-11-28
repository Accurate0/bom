use serde_json::Value;

/// An external identity (e.g. a forgejo actions job) the router admitted, from its passport.
#[derive(Debug, Clone)]
pub struct External {
    /// The issuer's configured name, e.g. `forgejo`.
    pub issuer: String,
    pub claims: Value,
    /// Scopes the claim patterns this token matched granted it.
    pub scopes: std::collections::BTreeSet<String>,
}

impl External {
    pub fn claims_as<T: serde::de::DeserializeOwned>(&self) -> async_graphql::Result<T> {
        serde_json::from_value(self.claims.clone()).map_err(|e| {
            async_graphql::Error::new(format!("unexpected {} claims: {e}", self.issuer))
        })
    }
}
