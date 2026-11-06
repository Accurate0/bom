//! Copied from crates/subgraph in the k8s repo, which bom can't depend on.

pub mod auth;
pub mod node;
pub mod server;

pub use auth::{AuthConfig, Authenticator};
pub use server::router;

#[cfg(test)]
pub fn assert_subgraph_current<Q, M, S>(
    schema: &async_graphql::Schema<Q, M, S>,
    path: impl AsRef<std::path::Path>,
) where
    Q: async_graphql::ObjectType + 'static,
    M: async_graphql::ObjectType + 'static,
    S: async_graphql::SubscriptionType + 'static,
{
    let sdl = schema.sdl_with_options(async_graphql::SDLExportOptions::new().federation());
    let path = path.as_ref();
    let current = std::fs::read_to_string(path).unwrap_or_default();

    if current != sdl {
        std::fs::write(path, &sdl).expect("write subgraph.graphql");
        panic!(
            "{} was out of date and has been regenerated, commit it",
            path.display()
        );
    }
}
