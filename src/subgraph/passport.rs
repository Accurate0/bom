use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const HEADER: &str = "x-graph-passport";
pub const ISSUER: &str = "graph-router";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentityKind {
    User,
    External,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Identity {
    pub issuer: String,
    pub kind: IdentityKind,
    pub subject: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub scopes: BTreeSet<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub claims: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Passport {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
    pub identity: Identity,
}
