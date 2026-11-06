use http::{HeaderMap, HeaderValue};
use opentelemetry::{global, trace::TracerProvider as _, KeyValue};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider},
    Resource,
};
use opentelemetry_semantic_conventions::resource::{
    DEPLOYMENT_ENVIRONMENT_NAME, SERVICE_NAME, TELEMETRY_SDK_LANGUAGE, TELEMETRY_SDK_NAME,
    TELEMETRY_SDK_VERSION,
};
use std::time::Duration;
use tracing::Level;
use tracing_subscriber::{filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};

const SERVICE: &str = "bom";

fn default_targets() -> Targets {
    Targets::default()
        .with_target("bom", Level::INFO)
        .with_target("otel::tracing", Level::TRACE)
        .with_default(Level::INFO)
}

fn build_provider(endpoint: String) -> SdkTracerProvider {
    let mut headers = HeaderMap::<HeaderValue>::with_capacity(1);
    headers.insert(
        "User-Agent",
        HeaderValue::from_str(&format!("{SERVICE}/{}", env!("CARGO_PKG_VERSION"))).unwrap(),
    );

    let tags = vec![
        KeyValue::new(TELEMETRY_SDK_NAME, "otel-tracing-rs".to_string()),
        KeyValue::new(TELEMETRY_SDK_VERSION, env!("CARGO_PKG_VERSION").to_string()),
        KeyValue::new(TELEMETRY_SDK_LANGUAGE, "rust".to_string()),
        KeyValue::new(SERVICE_NAME, SERVICE.to_string()),
        KeyValue::new(
            DEPLOYMENT_ENVIRONMENT_NAME,
            if cfg!(debug_assertions) {
                "development"
            } else {
                "production"
            },
        ),
    ];

    let resource = Resource::builder_empty().with_attributes(tags).build();

    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpJson)
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(3))
        .build()
        .expect("failed to build OTLP span exporter");

    SdkTracerProvider::builder()
        .with_span_processor(
            BatchSpanProcessor::builder(span_exporter)
                .with_batch_config(
                    BatchConfigBuilder::default()
                        .with_max_queue_size(20480)
                        .build(),
                )
                .build(),
        )
        .with_resource(resource)
        .build()
}

pub fn init() -> Option<SdkTracerProvider> {
    let endpoint = match std::env::var("OTEL_TRACING_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            tracing_subscriber::fmt::init();
            return None;
        }
    };

    let provider = build_provider(endpoint);
    let tracer = provider.tracer(SERVICE);

    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    global::set_tracer_provider(provider.clone());

    tracing_subscriber::registry()
        .with(default_targets())
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .init();

    Some(provider)
}
