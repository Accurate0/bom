//! Async-graphql extension ported from spotify: a `graphql.<operation>` span per operation and a
//! `graphql.<Type>.<field>` span per top-level field, nested under the request's HTTP span so the
//! router's trace continues into each subgraph.

use std::sync::Arc;

use async_graphql::{
    Response, ServerError, ServerResult, ValidationResult, Value, Variables,
    extensions::{
        Extension, ExtensionContext, ExtensionFactory, NextExecute, NextParseQuery, NextRequest,
        NextResolve, NextValidation, ResolveInfo,
    },
    parser::types::{ExecutableDocument, Selection},
};
use tracing::{Instrument, Span};

pub struct Tracing;

impl ExtensionFactory for Tracing {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(TracingExtension)
    }
}

struct TracingExtension;

fn record_error(span: &Span, message: &str) {
    span.record("otel.status_code", "ERROR");
    span.record("otel.status_message", message);
}

#[async_graphql::async_trait::async_trait]
impl Extension for TracingExtension {
    async fn request(&self, ctx: &ExtensionContext<'_>, next: NextRequest<'_>) -> Response {
        next.run(ctx)
            .instrument(tracing::span!(
                target: "async_graphql::graphql",
                tracing::Level::DEBUG,
                "graphql.request",
            ))
            .await
    }

    async fn parse_query(
        &self,
        ctx: &ExtensionContext<'_>,
        query: &str,
        variables: &Variables,
        next: NextParseQuery<'_>,
    ) -> ServerResult<ExecutableDocument> {
        let span = tracing::span!(
            target: "async_graphql::graphql",
            tracing::Level::INFO,
            "graphql.parse",
            source = tracing::field::Empty,
            operation = tracing::field::Empty,
            fields = tracing::field::Empty,
        );
        let res = next
            .run(ctx, query, variables)
            .instrument(span.clone())
            .await;

        if let Ok(doc) = &res {
            span.record("source", ctx.stringify_execute_doc(doc, variables).as_str());

            if let Some((name, operation)) = doc.operations.iter().next() {
                if let Some(name) = name {
                    span.record("operation", name.as_str());
                }

                let fields = operation
                    .node
                    .selection_set
                    .node
                    .items
                    .iter()
                    .filter_map(|item| match &item.node {
                        Selection::Field(field) => Some(field.node.name.node.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                span.record("fields", fields.as_str());
            }
        }
        res
    }

    async fn validation(
        &self,
        ctx: &ExtensionContext<'_>,
        next: NextValidation<'_>,
    ) -> Result<ValidationResult, Vec<ServerError>> {
        let span = tracing::span!(
            target: "async_graphql::graphql",
            tracing::Level::INFO,
            "graphql.validate"
        );
        next.run(ctx).instrument(span).await
    }

    async fn execute(
        &self,
        ctx: &ExtensionContext<'_>,
        operation_name: Option<&str>,
        next: NextExecute<'_>,
    ) -> Response {
        let operation = operation_name.unwrap_or("anonymous");
        let span = tracing::span!(
            target: "async_graphql::graphql",
            tracing::Level::INFO,
            "graphql.execute",
            otel.name = format!("graphql.{operation}"),
            operation = operation,
            otel.status_code = tracing::field::Empty,
            otel.status_message = tracing::field::Empty,
        );

        let response = next.run(ctx, operation_name).instrument(span.clone()).await;
        if let Some(error) = response.errors.first() {
            record_error(&span, &error.message);
        }
        response
    }

    async fn resolve(
        &self,
        ctx: &ExtensionContext<'_>,
        info: ResolveInfo<'_>,
        next: NextResolve<'_>,
    ) -> ServerResult<Option<Value>> {
        if info.is_for_introspection || info.path_node.parent.is_some() {
            let res = next.run(ctx, info).await;
            if let Err(err) = &res {
                tracing::error!(target: "async_graphql::graphql", error = %err.message, "error");
            }
            return res;
        }

        let span = tracing::span!(
            target: "async_graphql::graphql",
            tracing::Level::INFO,
            "graphql.field",
            otel.name = format!("graphql.{}.{}", info.parent_type, info.name),
            path = %info.path_node,
            return_type = info.return_type,
            otel.status_code = tracing::field::Empty,
            otel.status_message = tracing::field::Empty,
        );

        let res = next.run(ctx, info).instrument(span.clone()).await;
        if let Err(err) = &res {
            span.in_scope(|| {
                tracing::error!(target: "async_graphql::graphql", error = %err.message, "error");
            });
            record_error(&span, &err.message);
        }
        res
    }
}
