use std::fmt::Display;

use async_graphql::{Error, ID, Result};

/// Relay ids are `<service>:<node>:<key>`; the router's node plugin turns `<service>` and
/// `<node>` into the type name, so `<service>` is the subgraph prefix and `<node>` is the type
/// name without it, in lowerCamelCase. Keys may contain `:`.
#[derive(Debug, Clone, Copy)]
pub struct Ids {
    pub service: &'static str,
}

impl Ids {
    pub const fn new(service: &'static str) -> Self {
        Self { service }
    }

    pub fn encode(&self, node: &str, key: impl Display) -> ID {
        ID(format!("{}:{node}:{key}", self.service))
    }

    pub fn decode(&self, id: &ID) -> Result<(String, String)> {
        let mut parts = id.splitn(3, ':');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(service), Some(node), Some(key)) if service == self.service => {
                Ok((node.to_owned(), key.to_owned()))
            }
            _ => Err(invalid(id)),
        }
    }

    pub fn decode_as(&self, id: &ID, expected: &str) -> Result<String> {
        match self.decode(id)? {
            (node, key) if node == expected => Ok(key),
            _ => Err(Error::new(format!("expected a {expected} id"))),
        }
    }

    pub fn decode_row_id(&self, id: &ID, expected: &str) -> Result<i64> {
        self.decode_as(id, expected)?
            .parse()
            .map_err(|_| invalid(id))
    }
}

fn invalid(id: &ID) -> Error {
    Error::new(format!("invalid id {:?}", id.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: Ids = Ids::new("registry");

    #[test]
    fn round_trips() {
        let id = IDS.encode("version", "ortzi-app:0.47.0");
        assert_eq!(id.as_str(), "registry:version:ortzi-app:0.47.0");
        assert_eq!(IDS.decode_as(&id, "version").unwrap(), "ortzi-app:0.47.0");
        assert!(IDS.decode_as(&id, "package").is_err());
        assert!(IDS.decode(&ID("waf:block:x".into())).is_err());
        assert!(IDS.decode(&ID("nope".into())).is_err());
        assert_eq!(
            IDS.decode_row_id(&IDS.encode("token", 4), "token").unwrap(),
            4
        );
    }
}
