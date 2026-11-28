use async_graphql::http::ALL_WEBSOCKET_PROTOCOLS;
use async_graphql::{Data, ObjectType, Schema, SubscriptionType};
use async_graphql_axum::{GraphQLProtocol, GraphQLRequest, GraphQLResponse, GraphQLWebSocket};
use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::Value;

use crate::subgraph::auth::{AuthError, Authenticator, Principal};
use crate::subgraph::passport;

pub const GRAPHQL_PATH: &str = "/v1/graphql";
pub const WEBSOCKET_PATH: &str = "/v1/graphql/ws";

struct Subgraph<Q, M, S> {
    schema: Schema<Q, M, S>,
    auth: Authenticator,
}

impl<Q, M, S> Clone for Subgraph<Q, M, S> {
    fn clone(&self) -> Self {
        Self {
            schema: self.schema.clone(),
            auth: self.auth.clone(),
        }
    }
}

/// Serves the subgraph on GRAPHQL_PATH and WEBSOCKET_PATH. Every operation needs the router's
/// passport for this subgraph; resolvers read the caller's `Claims` (users) or `External`
/// (machines) from the context.
pub fn router<Q, M, S>(schema: Schema<Q, M, S>, auth: Authenticator) -> Router
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    Router::new()
        .route(GRAPHQL_PATH, post(execute::<Q, M, S>))
        .route(WEBSOCKET_PATH, get(subscribe::<Q, M, S>))
        .with_state(Subgraph { schema, auth })
}

async fn execute<Q, M, S>(
    State(subgraph): State<Subgraph<Q, M, S>>,
    headers: HeaderMap,
    request: GraphQLRequest,
) -> Result<GraphQLResponse, AuthError>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let principal = subgraph.auth.authenticate_headers(&headers).await?;
    let request = request.into_inner();
    let request = match principal {
        Principal::User(claims) => {
            tracing::debug!(user = claims.identity(), "authorized");
            request.data(claims)
        }
        Principal::External(external) => {
            tracing::debug!(issuer = external.issuer, "authorized external identity");
            request.data(external)
        }
    };

    Ok(subgraph.schema.execute(request).await.into())
}

async fn subscribe<Q, M, S>(
    State(subgraph): State<Subgraph<Q, M, S>>,
    protocol: GraphQLProtocol,
    upgrade: WebSocketUpgrade,
) -> Response
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    upgrade
        .protocols(ALL_WEBSOCKET_PROTOCOLS)
        .on_upgrade(move |stream| {
            let auth = subgraph.auth.clone();
            GraphQLWebSocket::new(stream, subgraph.schema.clone(), protocol)
                .on_connection_init(move |payload| async move {
                    let token = token_from_payload(&payload)
                        .ok_or_else(|| async_graphql::Error::new("missing graph passport"))?;
                    let principal = auth
                        .authenticate(&token)
                        .await
                        .map_err(|e| async_graphql::Error::new(e.to_string()))?;

                    let mut data = Data::default();
                    match principal {
                        Principal::User(claims) => data.insert(claims),
                        Principal::External(external) => data.insert(external),
                    }
                    Ok(data)
                })
                .serve()
        })
        .into_response()
}

/// The router sends its fetch headers, the passport among them, as the connection_init payload.
fn token_from_payload(payload: &Value) -> Option<String> {
    payload
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(passport::HEADER))
        .and_then(|(_, value)| value.as_str())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tokens() {
        let payload = serde_json::json!({ "x-graph-passport": "abc", "authorization": "Bearer t" });
        assert_eq!(token_from_payload(&payload).as_deref(), Some("abc"));
        assert_eq!(
            token_from_payload(&serde_json::json!({ "X-Graph-Passport": "abc" })).as_deref(),
            Some("abc")
        );
        assert_eq!(
            token_from_payload(&serde_json::json!({ "authorization": "Bearer t" })),
            None
        );
        assert_eq!(token_from_payload(&serde_json::json!({})), None);
    }

    #[tokio::test]
    async fn requests_without_a_token_are_rejected_as_graphql_errors() {
        use async_graphql::{EmptyMutation, EmptySubscription, Object};
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        struct Query;
        #[Object]
        impl Query {
            async fn ok(&self) -> bool {
                true
            }
        }

        let auth = Authenticator::new(crate::subgraph::AuthConfig {
            audience: "registry".to_owned(),
            jwks: "http://router.invalid/.well-known/passport.jwks".to_owned(),
            grants: Default::default(),
        });
        let app = router(Schema::new(Query, EmptyMutation, EmptySubscription), auth);
        let response = app
            .oneshot(
                Request::post(GRAPHQL_PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"{ ok }"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["errors"][0]["extensions"]["code"], "UNAUTHENTICATED");
    }
}
