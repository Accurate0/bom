use async_graphql::http::ALL_WEBSOCKET_PROTOCOLS;
use async_graphql::{Data, ObjectType, Schema, SubscriptionType};
use async_graphql_axum::{GraphQLProtocol, GraphQLRequest, GraphQLResponse, GraphQLWebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value;

use super::auth::{bearer, AuthError, Authenticator};

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

/// Serves the subgraph on GRAPHQL_PATH and WEBSOCKET_PATH. Every operation needs a Kanidm token
/// in an allowed group; resolvers read the verified `Claims` from the context.
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
    let claims = subgraph.auth.authorize_headers(&headers).await?;
    tracing::debug!(user = claims.identity(), "authorized");

    Ok(subgraph
        .schema
        .execute(request.into_inner().data(claims))
        .await
        .into())
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
                        .ok_or_else(|| async_graphql::Error::new("missing bearer token"))?;
                    let claims = auth
                        .authorize(&token)
                        .await
                        .map_err(|e| async_graphql::Error::new(e.to_string()))?;

                    let mut data = Data::default();
                    data.insert(claims);
                    Ok(data)
                })
                .serve()
        })
        .into_response()
}

/// The router forwards the client's headers as the connection_init payload.
fn token_from_payload(payload: &Value) -> Option<String> {
    ["Authorization", "authorization"]
        .iter()
        .find_map(|key| payload.get(key).and_then(Value::as_str))
        .and_then(bearer)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tokens() {
        let payload = serde_json::json!({ "authorization": "Bearer abc" });
        assert_eq!(token_from_payload(&payload).as_deref(), Some("abc"));
        assert_eq!(
            token_from_payload(&serde_json::json!({ "Authorization": "abc" })),
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

        let auth = Authenticator::new(super::super::auth::AuthConfig {
            providers: vec!["https://idm.example.test/oauth2/openid/graph=graph"
                .parse()
                .unwrap()],
            groups: vec!["registry_users".to_owned()],
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
