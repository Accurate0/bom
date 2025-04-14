use anyhow::Context;
use axum::{extract::State, http::StatusCode, routing::get};
use sqlx::{postgres::PgPoolOptions, Connection};
use std::{future::IntoFuture, ops::Deref, sync::Arc, time::Duration};
use tracing::Level;
use tracing_subscriber::{filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};
use twilight_cache_inmemory::{DefaultCacheModels, InMemoryCacheBuilder, ResourceType};
use twilight_gateway::{
    ConfigBuilder, Event, EventType, EventTypeFlags, Intents, Shard, ShardId, StreamExt,
};
use twilight_http::Client as HttpClient;
use twilight_model::{
    application::command::{CommandOptionChoice, CommandOptionChoiceValue},
    http::interaction::InteractionResponseData,
    util::Timestamp,
};
use twilight_util::builder::embed::{EmbedBuilder, ImageSource};
use vesper::{
    framework::DefaultError,
    macros::{autocomplete, command, error_handler},
    prelude::{AutocompleteContext, DefaultCommandResult, Framework, SlashContext},
};

mod background;
mod bom;

#[derive(Clone)]
struct BotContext(Arc<BotContextInner>);

impl Deref for BotContext {
    type Target = BotContextInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct BotContextInner {
    bom: Arc<bom::BOM>,
}

async fn handle_event(event: Event, _http: Arc<HttpClient>) -> anyhow::Result<()> {
    match event {
        Event::GatewayHeartbeatAck
        | Event::MessageCreate(_)
        | Event::MessageUpdate(_)
        | Event::MessageDelete(_) => {}
        // Other events here...
        e => {
            tracing::warn!("unhandled event: {e:?}")
        }
    }

    Ok(())
}

#[autocomplete]
async fn autocomplete_location(
    ctx: AutocompleteContext<BotContext>,
) -> Option<InteractionResponseData> {
    let choices = sqlx::query!(r#"SELECT * FROM locations"#)
        .fetch_all(ctx.data.bom.db())
        .await
        .ok()?
        .into_iter()
        .map(|item| CommandOptionChoice {
            name: item.name,
            name_localizations: None,
            value: CommandOptionChoiceValue::String(item.bom_radar_id.to_string()),
        })
        .collect();

    Some(InteractionResponseData {
        choices: Some(choices),
        ..Default::default()
    })
}

#[error_handler]
async fn handle_interaction_error(ctx: &mut SlashContext<BotContext>, error: DefaultError) {
    let fut = async {
        let error = if error.to_string().contains("Missing Access") {
            "This channel is not accessible to the bot...".to_string()
        } else {
            error.to_string()
        };

        let embed = EmbedBuilder::new()
            .title("oops")
            .description(error)
            .color(0xcc6666)
            .validate()?
            .build();

        ctx.interaction_client
            .update_response(&ctx.interaction.token)
            .embeds(Some(&[embed]))
            .await?;

        Ok::<(), anyhow::Error>(())
    };

    if let Err(e) = fut.await {
        tracing::error!("error in updating message: {e:?}");
    }

    tracing::error!("error in interaction: {error:?}");
}

#[command]
#[only_guilds]
#[description = "get radar images from bom"]
#[error_handler(handle_interaction_error)]
async fn radar(
    ctx: &mut SlashContext<BotContext>,
    #[autocomplete(autocomplete_location)]
    #[description = "pick a location"]
    location: Option<String>,
) -> DefaultCommandResult {
    ctx.defer(false).await?;

    // perth
    let location = location.unwrap_or_else(|| "IDR703".to_owned());
    let location_name = sqlx::query!(
        "SELECT name FROM locations WHERE bom_radar_id = ($1)",
        location
    )
    .fetch_one(ctx.data.bom.db())
    .await?;

    let url = ctx.data.bom.generate_radar_gif_for(&location).await?;

    let now = chrono::offset::Utc::now().naive_utc();
    let embed = EmbedBuilder::new()
        .title(location_name.name)
        .color(0x003366)
        .timestamp(
            Timestamp::from_secs(now.and_utc().timestamp())
                .context("must have valid time")
                .unwrap(),
        );

    let image = ImageSource::url(url);

    let embed = match image {
        Ok(image) => embed.image(image),
        Err(_) => embed,
    }
    .build();

    ctx.interaction_client
        .update_response(&ctx.interaction.token)
        .embeds(Some(&[embed]))
        .await?;

    Ok(())
}

async fn health(ctx: State<BotContext>) -> StatusCode {
    let resp = ctx.bom.db().acquire().await;

    if resp.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    let resp = resp.unwrap().ping().await;
    match resp {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(Targets::default().with_default(Level::INFO))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let database_url = std::env::var("DATABASE_URL")?;
    let token = std::env::var("DISCORD_TOKEN")?;

    let access_key_id = std::env::var("BUCKET_ACCESS_KEY_ID")?;
    let access_secret_key = std::env::var("BUCKET_ACCESS_SECRET_KEY")?;
    let bucket_name = std::env::var("BUCKET_NAME")?;
    let bucket_endpoint = std::env::var("BUCKET_ENDPOINT")?;

    let credentials = s3::creds::Credentials::new(
        Some(&access_key_id),
        Some(&access_secret_key),
        None,
        None,
        None,
    )?;

    let bucket = s3::Bucket::new(
        &bucket_name,
        s3::Region::Custom {
            region: "".to_owned(),
            endpoint: bucket_endpoint,
        },
        credentials,
    )?;

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    let bom = Arc::new(bom::BOM::new(bucket, pool).await?);
    bom.generate_radar_backgrounds().await?;

    let context = BotContext(BotContextInner { bom: bom.clone() }.into());

    let config = ConfigBuilder::new(
        token.clone(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT,
    )
    .build();

    let mut shard = Shard::with_config(ShardId::ONE, config);

    let http = Arc::new(HttpClient::new(token));

    let cache = InMemoryCacheBuilder::<DefaultCacheModels>::new()
        .resource_types(ResourceType::MESSAGE | ResourceType::GUILD)
        .build();

    let app = axum::Router::new()
        .route("/health", get(health))
        .with_state(context.clone());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    tracing::info!("spawning axum");
    tokio::spawn(axum::serve(listener, app).into_future());

    tracing::info!("spawning background thread");
    let bom_cloned = bom.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = background::refresh_all_images(bom_cloned.clone()).await {
                tracing::info!("error in refresh: {e}");
            }

            if let Err(e) = background::cleanup_old_images(bom_cloned.clone()).await {
                tracing::info!("error in cleanup: {e}");
            }

            tokio::time::sleep(Duration::from_secs(900)).await;
        }
    });

    let app_id = http.current_user_application().await?.model().await?.id;

    let framework = Arc::new(
        Framework::builder(Arc::clone(&http), app_id, context)
            .command(radar)
            .build(),
    );

    framework.register_global_commands().await?;

    tracing::info!("starting event loop");
    while let Some(event) = shard.next_event(EventTypeFlags::all()).await {
        let Ok(event) = event else {
            let source = event.unwrap_err();
            tracing::warn!(source = ?source, "error receiving event");

            continue;
        };

        if matches!(event.kind(), EventType::GatewayHeartbeatAck) {
            continue;
        }

        cache.update(&event);

        if matches!(event.kind(), EventType::Ready) {
            tracing::info!("connected on shard");
            continue;
        }

        cache.update(&event);

        if let Event::InteractionCreate(i) = event {
            let clone = Arc::clone(&framework);
            tokio::spawn(async move {
                let inner = i.0;
                clone.process(inner).await;
            });

            continue;
        }

        tokio::spawn(handle_event(event, Arc::clone(&http)));
    }

    Ok(())
}
