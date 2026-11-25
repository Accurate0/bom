//! Enforces `#[graphql(requires_scopes = "...")]` in the subgraph itself: the router checks the
//! same scopes, but every layer authorizes on its own. Each entry is one alternative whose
//! space-separated scopes must all be held.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_graphql::extensions::{
    Extension, ExtensionContext, ExtensionFactory, NextResolve, ResolveInfo,
};
use async_graphql::registry::MetaType;
use async_graphql::{Context, ErrorExtensions, ServerResult, Value};

use crate::subgraph::{Claims, External};

/// Add with `Schema::build(..).extension(subgraph::Scopes)`.
pub struct Scopes;

impl ExtensionFactory for Scopes {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(ScopesExtension)
    }
}

struct ScopesExtension;

fn held(claims: Option<&Claims>, external: Option<&External>) -> BTreeSet<String> {
    match (claims, external) {
        (Some(claims), _) => claims.scopes().into_iter().map(str::to_owned).collect(),
        (None, Some(external)) => external.scopes.clone(),
        (None, None) => BTreeSet::new(),
    }
}

fn satisfied(required: &[String], held: &BTreeSet<String>) -> bool {
    required.is_empty()
        || required
            .iter()
            .any(|all| all.split_whitespace().all(|scope| held.contains(scope)))
}

fn type_scopes(meta: &MetaType) -> &[String] {
    match meta {
        MetaType::Object {
            requires_scopes, ..
        }
        | MetaType::Interface {
            requires_scopes, ..
        } => requires_scopes,
        _ => &[],
    }
}

fn missing(required: &[String]) -> async_graphql::Error {
    async_graphql::Error::new(format!("missing scope {}", required.join(" or ")))
        .extend_with(|_, e| e.set("code", "FORBIDDEN"))
}

#[async_graphql::async_trait::async_trait]
impl Extension for ScopesExtension {
    async fn resolve(
        &self,
        ctx: &ExtensionContext<'_>,
        info: ResolveInfo<'_>,
        next: NextResolve<'_>,
    ) -> ServerResult<Option<Value>> {
        if !info.is_for_introspection
            && let Some(parent) = ctx.schema_env.registry.types.get(info.parent_type)
        {
            let held = held(ctx.data_opt::<Claims>(), ctx.data_opt::<External>());
            let field = parent
                .field_by_name(info.name)
                .map(|field| field.requires_scopes.as_slice())
                .unwrap_or_default();

            for required in [type_scopes(parent), field] {
                if !satisfied(required, &held) {
                    return Err(missing(required).into_server_error(info.field.name.pos));
                }
            }
        }

        next.run(ctx, info).await
    }
}

/// For checks a resolver makes itself, e.g. only for some arguments.
pub fn require_scope(ctx: &Context<'_>, scope: &str) -> async_graphql::Result<()> {
    let held = held(ctx.data_opt::<Claims>(), ctx.data_opt::<External>());
    if held.contains(scope) {
        Ok(())
    } else {
        Err(missing(&[scope.to_owned()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_graphql::{EmptyMutation, EmptySubscription, Object, Request, Schema};

    struct Query;

    #[Object]
    impl Query {
        async fn open(&self) -> bool {
            true
        }

        #[graphql(requires_scopes = "registry:token:create")]
        async fn tokens(&self) -> bool {
            true
        }
    }

    fn claims(scope: &str) -> Claims {
        serde_json::from_value(serde_json::json!({ "sub": "u", "scope": scope })).unwrap()
    }

    #[tokio::test]
    async fn fields_need_their_scopes() {
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .extension(Scopes)
            .finish();

        let denied = schema
            .execute(Request::new("{ open tokens }").data(claims("openid registry:package:read")))
            .await;
        assert_eq!(
            denied.errors[0].message,
            "missing scope registry:token:create"
        );
        assert_eq!(
            denied.errors[0].extensions.as_ref().unwrap().get("code"),
            Some(&Value::from("FORBIDDEN"))
        );

        let allowed = schema
            .execute(Request::new("{ open tokens }").data(claims("openid registry:token:create")))
            .await;
        assert!(allowed.errors.is_empty(), "{:?}", allowed.errors);

        let external = External {
            issuer: "forgejo".to_owned(),
            claims: serde_json::json!({}),
            scopes: ["registry:token:create".to_owned()].into(),
        };
        let ci = schema
            .execute(Request::new("{ tokens }").data(external))
            .await;
        assert!(ci.errors.is_empty(), "{:?}", ci.errors);

        assert!(
            schema
                .sdl_with_options(async_graphql::SDLExportOptions::new().federation())
                .contains(
                    r#"tokens: Boolean! @requiresScopes(scopes: [["registry:token:create"]])"#
                )
        );
    }

    #[test]
    fn any_alternative_with_all_its_scopes_is_enough() {
        let held: BTreeSet<String> = ["registry:package:read".to_owned()].into();
        assert!(satisfied(&[], &held));
        assert!(satisfied(&["registry:package:read".to_owned()], &held));
        assert!(satisfied(
            &[
                "registry:token:create".to_owned(),
                "registry:package:read".to_owned()
            ],
            &held
        ));
        assert!(!satisfied(
            &["registry:package:read registry:token:create".to_owned()],
            &held
        ));
    }
}
