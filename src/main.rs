use crate::{
    types::{AppError, ForecastEndpointResponse, ForecastForDay},
    willyweather::WillyWeatherAPI,
};
use anyhow::Context;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Json,
};
use chrono::{DateTime, Utc};
use phf::phf_map;
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use std::{future::IntoFuture, ops::Deref, sync::Arc, time::Duration};
use tracing::Level;
use tracing_subscriber::{filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};
use twilight_cache_inmemory::{DefaultCacheModels, InMemoryCacheBuilder, ResourceType};
use twilight_gateway::{
    ConfigBuilder, Event, EventType, EventTypeFlags, Intents, Shard, ShardId, StreamExt,
};
use twilight_http::Client as HttpClient;
use twilight_model::{
    application::{
        command::{CommandOptionChoice, CommandOptionChoiceValue},
        interaction::InteractionContextType,
    },
    http::{attachment::Attachment, interaction::InteractionResponseData},
    oauth::ApplicationIntegrationType,
    util::Timestamp,
};
use twilight_util::builder::{
    command::CommandBuilder,
    embed::{EmbedBuilder, EmbedFieldBuilder, EmbedFooterBuilder, ImageSource},
};
use vesper::{
    framework::DefaultError,
    macros::{autocomplete, command, error_handler},
    prelude::{AutocompleteContext, DefaultCommandResult, Framework, SlashContext},
};

mod background;
mod bom;
mod types;
mod willyweather;

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
    willyweather: WillyWeatherAPI,
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
async fn autocomplete_location_forecast(
    _ctx: AutocompleteContext<BotContext>,
) -> Option<InteractionResponseData> {
    let choices = WillyWeatherAPI::get_locations()
        .into_iter()
        .map(|item| CommandOptionChoice {
            name: item.0,
            name_localizations: None,
            value: CommandOptionChoiceValue::String(item.1),
        })
        .collect();

    Some(InteractionResponseData {
        choices: Some(choices),
        ..Default::default()
    })
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
#[description = "get radar timelapse for 24h"]
#[error_handler(handle_interaction_error)]
async fn timelapse(
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

    let (url, bytes) = ctx.data.bom.get_radar_timelapse_24hr_for(&location).await?;

    let now = chrono::offset::Utc::now().naive_utc();
    let embed = EmbedBuilder::new()
        .title(format!("{} 24hr timelapse", location_name.name))
        .color(0x003366)
        .timestamp(
            Timestamp::from_secs(now.and_utc().timestamp())
                .context("must have valid time")
                .unwrap(),
        );

    tracing::info!("using url: {url}");

    let image = ImageSource::attachment("url.gif");

    let embed = match image {
        Ok(image) => embed.image(image),
        Err(e) => {
            tracing::error!("error with image url: {e}");
            embed
        }
    }
    .build();

    let attachment = Attachment::from_bytes("url.gif".to_owned(), bytes, 1);
    ctx.interaction_client
        .update_response(&ctx.interaction.token)
        .embeds(Some(&[embed]))
        .attachments(&[attachment])
        .await?;

    Ok(())
}

#[command]
#[description = "get satellite images from bom"]
#[error_handler(handle_interaction_error)]
async fn satellite(ctx: &mut SlashContext<BotContext>) -> DefaultCommandResult {
    ctx.defer(false).await?;

    // some satellite
    let location = "IDE00416";
    let location_name = sqlx::query!(
        "SELECT name FROM satellites WHERE bom_satellite_id = ($1)",
        location
    )
    .fetch_one(ctx.data.bom.db())
    .await?;

    let (url, bytes) = ctx.data.bom.get_latest_satellite_gif_for(location).await?;

    let now = chrono::offset::Utc::now().naive_utc();
    let embed = EmbedBuilder::new()
        .title(location_name.name)
        .color(0x003366)
        .timestamp(
            Timestamp::from_secs(now.and_utc().timestamp())
                .context("must have valid time")
                .unwrap(),
        );

    tracing::info!("using url: {url}");

    let image = ImageSource::attachment("url.gif");

    let embed = match image {
        Ok(image) => embed.image(image),
        Err(e) => {
            tracing::error!("error with image url: {e}");
            embed
        }
    }
    .build();

    let attachment = Attachment::from_bytes("url.gif".to_owned(), bytes, 1);
    ctx.interaction_client
        .update_response(&ctx.interaction.token)
        .embeds(Some(&[embed]))
        .attachments(&[attachment])
        .await?;

    Ok(())
}

#[command]
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

    let (url, bytes) = ctx.data.bom.generate_radar_gif_for(&location).await?;

    let now = chrono::offset::Utc::now().naive_utc();
    let embed = EmbedBuilder::new()
        .title(location_name.name)
        .color(0x003366)
        .timestamp(
            Timestamp::from_secs(now.and_utc().timestamp())
                .context("must have valid time")
                .unwrap(),
        );

    tracing::info!("using url: {url}");

    let image = ImageSource::attachment("url.gif");

    let embed = match image {
        Ok(image) => embed.image(image),
        Err(e) => {
            tracing::error!("error in radar embed {e}");
            embed
        }
    }
    .build();

    let attachment = Attachment::from_bytes("url.gif".to_owned(), bytes, 1);
    ctx.interaction_client
        .update_response(&ctx.interaction.token)
        .attachments(&[attachment])
        .embeds(Some(&[embed]))
        .await?;

    Ok(())
}

const PRECIS_TO_EMOJI: phf::Map<&'static str, &'static str> = phf_map! {
    "fine" => "☀️",
    "mostly-fine" => "🌤️",
    "high-cloud" => "☁️",
    "partly-cloudy" => "⛅",
    "mostly-cloudy" => "🌥️",
    "cloudy" => "☁️",
    "overcast" => "🌫️",
    "shower-or-two" => "🌦️",
    "chance-shower-fine" => "🌧️",
    "chance-shower-cloud" => "🌧️",
    "drizzle" => "🌧️",
    "few-showers" => "🌦️",
    "showers-rain" => "🌧️",
    "heavy-showers-rain" => "🌧️",
    "chance-thunderstorm-fine" => "⛈️",
    "chance-thunderstorm-cloud" => "⛈️",
    "chance-thunderstorm-showers" => "⛈️",
    "thunderstorm" => "⛈️",
    "chance-snow-fine" => "🌨️",
    "chance-snow-cloud" => "🌨️",
    "snow-and-rain" => "🌨️",
    "light-snow" => "🌨️",
    "snow" => "❄️",
    "heavy-snow" => "🌨️",
    "wind" => "💨",
    "frost" => "🧊",
    "fog" => "🌁",
    "hail" => "🌨️",
    "dust" => "🌪️",
};

#[command]
#[description = "get forecast information from bom"]
#[error_handler(handle_interaction_error)]
async fn forecast(
    ctx: &mut SlashContext<BotContext>,
    #[autocomplete(autocomplete_location_forecast)]
    #[description = "pick a location"]
    location: Option<String>,
    #[description = "number of days"] days: Option<i64>,
) -> DefaultCommandResult {
    ctx.defer(false).await?;

    // perth
    let location = location.unwrap_or_else(|| WillyWeatherAPI::PERTH_ID.to_owned());
    let days = days.unwrap_or(7);

    let forecast = ctx.data.willyweather.get_forecast(&location, &days).await?;

    let mut embed = EmbedBuilder::new()
        .title(format!("🌡️ Forecast for {}", forecast.location.name))
        .color(0x003366)
        .footer(EmbedFooterBuilder::new(
            "BOM charges $4,037.00 for this data",
        ));

    for (i, days) in forecast.forecasts.weather.days.into_iter().enumerate() {
        let entry = days.entries.first().context("must have entries")?;
        let min = entry.min;
        let max = entry.max;
        let description = &entry.precis;
        let datetime_with_timezone = &format!("{} +0800", entry.date_time);
        let datetime = DateTime::parse_from_str(datetime_with_timezone, "%Y-%m-%d %H:%M:%S %z")?;
        let emoji = PRECIS_TO_EMOJI.get(&entry.precis_code).map_or("", |e| e);
        let uv_level = forecast.forecasts.uv.days.get(i).map(|e| &e.alert);

        let formatted_date = if datetime.date_naive() == Utc::now().date_naive() {
            "Today".to_owned()
        } else {
            datetime.format("%A %d/%m").to_string()
        };

        let temperature_details = if let Some(uv_level) = uv_level {
            format!(
                "**Max:** {}°c, **Min:** {}°c, **UV:** {:.1}",
                max, min, uv_level.max_index
            )
        } else {
            format!("**Max:** {}°c, **Min:** {}°c", max, min)
        };

        embed = embed.field(
            EmbedFieldBuilder::new(
                formatted_date,
                format!("{} — {} {}", temperature_details, emoji, description),
            )
            .build(),
        )
    }

    ctx.interaction_client
        .update_response(&ctx.interaction.token)
        .embeds(Some(&[embed.build()]))
        .await?;

    Ok(())
}

async fn health(_ctx: State<BotContext>) -> StatusCode {
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct ForecastParams {
    location: Option<String>,
}

async fn forecast_endpoint(
    ctx: State<BotContext>,
    params: Query<ForecastParams>,
) -> Result<Json<ForecastEndpointResponse>, AppError> {
    let location = params
        .location
        .clone()
        .unwrap_or_else(|| WillyWeatherAPI::PERTH_ID.to_owned());

    let forecast_resp = ctx.willyweather.get_forecast(&location, &7).await?;
    let mut forecast_for_days = Vec::with_capacity(7);

    for (i, days) in forecast_resp.forecasts.weather.days.into_iter().enumerate() {
        let entry = days
            .entries
            .into_iter()
            .next()
            .context("must have entries")?;
        let min = entry.min;
        let max = entry.max;
        let description = entry.precis;
        let datetime_with_timezone = &format!("{} +0800", entry.date_time);
        let datetime = DateTime::parse_from_str(datetime_with_timezone, "%Y-%m-%d %H:%M:%S %z")?;
        let emoji = PRECIS_TO_EMOJI
            .get(&entry.precis_code)
            .map_or("", |e| e)
            .to_string();
        let uv_level = forecast_resp.forecasts.uv.days.get(i).map(|e| &e.alert);

        forecast_for_days.push(ForecastForDay {
            date_time: datetime.to_rfc3339(),
            code: entry.precis_code,
            emoji,
            description,
            min,
            max,
            uv: uv_level.map(|uv| uv.max_index),
        });
    }

    Ok(Json(ForecastEndpointResponse {
        days: forecast_for_days,
    }))
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
    let willyweather_api_key = std::env::var("WILLYWEATHER_API_KEY")?;

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
    )?
    .with_path_style();

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    let willyweather = WillyWeatherAPI::new(willyweather_api_key);
    let bom = Arc::new(bom::BOM::new(bucket, pool).await?);
    bom.generate_radar_backgrounds().await?;

    let context = BotContext(
        BotContextInner {
            bom: bom.clone(),
            willyweather,
        }
        .into(),
    );

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
        .route("/forecast", get(forecast_endpoint))
        .with_state(context.clone());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    tracing::info!("spawning axum");
    tokio::spawn(axum::serve(listener, app).into_future());

    // dont look
    if !cfg!(debug_assertions) {
        tracing::info!("spawning background thread");
        let bom_cloned = bom.clone();
        tokio::spawn(async move {
            loop {
                let bom_cloned = bom_cloned.clone();
                if let Err(e) = background::refresh_all_images(bom_cloned.clone()).await {
                    tracing::info!("error in refresh: {e}");
                }

                if let Err(e) = background::cleanup_old_images(bom_cloned.clone()).await {
                    tracing::info!("error in cleanup: {e}");
                }

                tokio::time::sleep(Duration::from_secs(900)).await;
            }
        });
    }

    let app_id = http.current_user_application().await?.model().await?.id;

    let framework = Arc::new(
        Framework::builder(Arc::clone(&http), app_id, context)
            .command(radar)
            .command(satellite)
            .command(timelapse)
            .command(forecast)
            .build(),
    );

    framework.register_global_commands().await?;
    let interaction_client = http.interaction(app_id);
    let global_commands = interaction_client.global_commands().await?.model().await?;

    let mut updated_commands = Vec::with_capacity(global_commands.len());
    for global_command in global_commands {
        let mut command = CommandBuilder::new(
            global_command.name,
            global_command.description,
            global_command.kind,
        )
        .integration_types(vec![
            ApplicationIntegrationType::GuildInstall,
            ApplicationIntegrationType::UserInstall,
        ])
        .contexts(vec![
            InteractionContextType::BotDm,
            InteractionContextType::PrivateChannel,
            InteractionContextType::Guild,
        ]);

        for option in global_command.options {
            command = command.option(option);
        }

        updated_commands.push(command.build());
    }

    tracing::info!("updating commands: {}", updated_commands.len());
    interaction_client
        .set_global_commands(&updated_commands)
        .await?;

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
