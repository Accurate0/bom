use std::sync::Arc;
use std::time::Duration;

use async_graphql::futures_util::Stream;
use async_graphql::{
    ComplexObject, Context, EmptyMutation, Error, Interface, Object, Result, Schema, SchemaBuilder,
    SimpleObject, Subscription, ID,
};
use chrono::{DateTime, Utc};

use crate::bom::{BOM, RADAR_CACHE_PATH, SATELLITE_CACHE_PATH};
use crate::images::{frame_timestamps, parse_timestamp, Image};
use crate::subgraph::node::Ids;
use crate::subgraph::{AuthConfig, Authenticator};
use crate::BotContext;

const IDS: Ids = Ids::new("bom");
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Origin the public image routes are served from, e.g. `https://bom.anurag.sh`.
pub struct PublicUrl(pub String);

pub fn builder() -> SchemaBuilder<QueryRoot, EmptyMutation, SubscriptionRoot> {
    Schema::build(QueryRoot, EmptyMutation, SubscriptionRoot)
        .register_output_type::<Node>()
        .enable_federation()
        .enable_subscription_in_federation()
        .extension(crate::subgraph::Tracing)
}

pub fn router(context: BotContext, public_url: String, auth: AuthConfig) -> axum::Router {
    let auth = Authenticator::new(auth);
    tokio::spawn(auth.clone().run());

    let schema = builder().data(context).data(PublicUrl(public_url)).finish();
    crate::subgraph::router(schema, auth)
}

fn bom<'a>(ctx: &Context<'a>) -> &'a Arc<BOM> {
    &ctx.data_unchecked::<BotContext>().bom
}

fn url(ctx: &Context<'_>, image: &Image) -> String {
    format!("{}{}", ctx.data_unchecked::<PublicUrl>().0, image.path())
}

fn internal(e: impl std::fmt::Display) -> Error {
    tracing::error!("graphql request failed: {e}");
    Error::new("internal error")
}

#[derive(Interface)]
#[graphql(field(name = "id", ty = "ID"))]
pub enum Node {
    RadarLocation(RadarLocation),
    RadarFrame(RadarFrame),
    Satellite(Satellite),
    SatelliteFrame(SatelliteFrame),
}

#[derive(SimpleObject)]
#[graphql(name = "BomRadarLocation", complex)]
pub struct RadarLocation {
    /// The BOM product id, e.g. IDR703.
    pub radar_id: String,
    pub name: String,
}

#[ComplexObject]
impl RadarLocation {
    async fn id(&self) -> ID {
        IDS.encode("radarLocation", &self.radar_id)
    }

    /// The map underlay (background, topography, place names) frames are drawn over.
    async fn base_image_url(&self, ctx: &Context<'_>) -> String {
        url(
            ctx,
            &Image::RadarBase {
                radar: self.radar_id.clone(),
            },
        )
    }

    /// Cached frames, oldest first; the newest `limit` of them.
    async fn frames(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 20)] limit: usize,
    ) -> Result<Vec<RadarFrame>> {
        let frames = RadarFrame::list(ctx, &self.radar_id).await?;
        let skip = frames.len().saturating_sub(limit);
        Ok(frames.into_iter().skip(skip).collect())
    }

    async fn latest_frame(&self, ctx: &Context<'_>) -> Result<Option<RadarFrame>> {
        Ok(RadarFrame::list(ctx, &self.radar_id).await?.pop())
    }
}

impl RadarLocation {
    async fn list(ctx: &Context<'_>) -> Result<Vec<Self>> {
        sqlx::query_as!(
            RadarLocation,
            "SELECT bom_radar_id AS radar_id, name FROM locations ORDER BY id"
        )
        .fetch_all(bom(ctx).db())
        .await
        .map_err(internal)
    }

    async fn load(ctx: &Context<'_>, radar_id: &str) -> Result<Self> {
        sqlx::query_as!(
            RadarLocation,
            "SELECT bom_radar_id AS radar_id, name FROM locations WHERE bom_radar_id = $1",
            radar_id
        )
        .fetch_optional(bom(ctx).db())
        .await
        .map_err(internal)?
        .ok_or_else(|| Error::new(format!("radar {radar_id} not found")))
    }
}

#[derive(SimpleObject, Clone)]
#[graphql(name = "BomRadarFrame", complex)]
pub struct RadarFrame {
    pub radar_id: String,
    pub captured_at: DateTime<Utc>,
    /// Public, cacheable PNG; transparent rain over the location's base image.
    pub url: String,
    #[graphql(skip)]
    pub timestamp: String,
}

#[ComplexObject]
impl RadarFrame {
    async fn id(&self) -> ID {
        IDS.encode(
            "radarFrame",
            format!("{}.{}", self.radar_id, self.timestamp),
        )
    }

    async fn location(&self, ctx: &Context<'_>) -> Result<RadarLocation> {
        RadarLocation::load(ctx, &self.radar_id).await
    }
}

impl RadarFrame {
    fn new(ctx: &Context<'_>, radar_id: &str, timestamp: String) -> Option<Self> {
        let image = Image::RadarFrame {
            radar: radar_id.to_owned(),
            timestamp: timestamp.clone(),
        };
        Some(Self {
            radar_id: radar_id.to_owned(),
            captured_at: parse_timestamp(&timestamp)?,
            url: url(ctx, &image),
            timestamp,
        })
    }

    async fn list(ctx: &Context<'_>, radar_id: &str) -> Result<Vec<Self>> {
        let prefix = format!("{RADAR_CACHE_PATH}/{radar_id}.T.");
        Ok(frame_timestamps(bom(ctx), &prefix, ".png")
            .await
            .map_err(internal)?
            .into_iter()
            .filter_map(|timestamp| Self::new(ctx, radar_id, timestamp))
            .collect())
    }
}

#[derive(SimpleObject)]
#[graphql(name = "BomSatellite", complex)]
pub struct Satellite {
    /// The BOM product id, e.g. IDE00416.
    pub satellite_id: String,
    pub name: String,
}

#[ComplexObject]
impl Satellite {
    async fn id(&self) -> ID {
        IDS.encode("satellite", &self.satellite_id)
    }

    /// An animated loop of the latest frames, refreshed in the background.
    async fn latest_gif_url(&self, ctx: &Context<'_>) -> String {
        url(
            ctx,
            &Image::SatelliteLatest {
                satellite: self.satellite_id.clone(),
            },
        )
    }

    async fn frames(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 20)] limit: usize,
    ) -> Result<Vec<SatelliteFrame>> {
        let prefix = format!("{SATELLITE_CACHE_PATH}/{}.", self.satellite_id);
        let timestamps = frame_timestamps(bom(ctx), &prefix, ".jpg")
            .await
            .map_err(internal)?;
        let skip = timestamps.len().saturating_sub(limit);
        Ok(timestamps
            .into_iter()
            .skip(skip)
            .filter_map(|timestamp| SatelliteFrame::new(ctx, &self.satellite_id, timestamp))
            .collect())
    }
}

impl Satellite {
    async fn list(ctx: &Context<'_>) -> Result<Vec<Self>> {
        sqlx::query_as!(
            Satellite,
            "SELECT bom_satellite_id AS satellite_id, name FROM satellites ORDER BY id"
        )
        .fetch_all(bom(ctx).db())
        .await
        .map_err(internal)
    }
}

#[derive(SimpleObject)]
#[graphql(name = "BomSatelliteFrame", complex)]
pub struct SatelliteFrame {
    pub satellite_id: String,
    pub captured_at: DateTime<Utc>,
    pub url: String,
    #[graphql(skip)]
    pub timestamp: String,
}

#[ComplexObject]
impl SatelliteFrame {
    async fn id(&self) -> ID {
        IDS.encode(
            "satelliteFrame",
            format!("{}.{}", self.satellite_id, self.timestamp),
        )
    }
}

impl SatelliteFrame {
    fn new(ctx: &Context<'_>, satellite_id: &str, timestamp: String) -> Option<Self> {
        let image = Image::SatelliteFrame {
            satellite: satellite_id.to_owned(),
            timestamp: timestamp.clone(),
        };
        Some(Self {
            satellite_id: satellite_id.to_owned(),
            captured_at: parse_timestamp(&timestamp)?,
            url: url(ctx, &image),
            timestamp,
        })
    }
}

#[derive(SimpleObject)]
#[graphql(name = "BomForecastDay")]
pub struct ForecastDay {
    pub date_time: String,
    pub code: String,
    pub description: String,
    pub emoji: String,
    pub min: i64,
    pub max: i64,
    pub uv: Option<f64>,
}

fn frame_key(key: &str) -> Result<(&str, &str)> {
    key.split_once('.')
        .ok_or_else(|| Error::new(format!("invalid frame key {key:?}")))
}

pub struct QueryRoot;

#[Object(name = "Query")]
impl QueryRoot {
    async fn bom_radar_locations(&self, ctx: &Context<'_>) -> Result<Vec<RadarLocation>> {
        RadarLocation::list(ctx).await
    }

    async fn bom_radar_location(
        &self,
        ctx: &Context<'_>,
        radar_id: String,
    ) -> Result<RadarLocation> {
        RadarLocation::load(ctx, &radar_id).await
    }

    async fn bom_satellites(&self, ctx: &Context<'_>) -> Result<Vec<Satellite>> {
        Satellite::list(ctx).await
    }

    /// Seven days from WillyWeather; `location` is a WillyWeather id, Perth by default.
    async fn bom_forecast(
        &self,
        ctx: &Context<'_>,
        location: Option<String>,
    ) -> Result<Vec<ForecastDay>> {
        let context = ctx.data_unchecked::<BotContext>();
        let location = location.unwrap_or_else(|| crate::WillyWeatherAPI::PERTH_ID.to_owned());
        let days = crate::forecast_days(&context.willyweather, &location)
            .await
            .map_err(internal)?;
        Ok(days
            .into_iter()
            .map(|d| ForecastDay {
                date_time: d.date_time,
                code: d.code,
                description: d.description,
                emoji: d.emoji,
                min: d.min,
                max: d.max,
                uv: d.uv,
            })
            .collect())
    }

    #[graphql(entity)]
    async fn find_radar_location_by_id(&self, ctx: &Context<'_>, id: ID) -> Result<RadarLocation> {
        RadarLocation::load(ctx, &IDS.decode_as(&id, "radarLocation")?).await
    }

    #[graphql(entity)]
    async fn find_radar_frame_by_id(&self, ctx: &Context<'_>, id: ID) -> Result<RadarFrame> {
        let key = IDS.decode_as(&id, "radarFrame")?;
        let (radar_id, timestamp) = frame_key(&key)?;
        RadarFrame::new(ctx, radar_id, timestamp.to_owned())
            .ok_or_else(|| Error::new(format!("invalid id {:?}", id.as_str())))
    }

    #[graphql(entity)]
    async fn find_satellite_by_id(&self, ctx: &Context<'_>, id: ID) -> Result<Satellite> {
        let satellite_id = IDS.decode_as(&id, "satellite")?;
        Satellite::list(ctx)
            .await?
            .into_iter()
            .find(|s| s.satellite_id == satellite_id)
            .ok_or_else(|| Error::new(format!("satellite {satellite_id} not found")))
    }

    #[graphql(entity)]
    async fn find_satellite_frame_by_id(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> Result<SatelliteFrame> {
        let key = IDS.decode_as(&id, "satelliteFrame")?;
        let (satellite_id, timestamp) = frame_key(&key)?;
        SatelliteFrame::new(ctx, satellite_id, timestamp.to_owned())
            .ok_or_else(|| Error::new(format!("invalid id {:?}", id.as_str())))
    }
}

pub struct SubscriptionRoot;

#[Subscription(name = "Subscription")]
impl SubscriptionRoot {
    /// New frames for the radar as the background refresh caches them.
    async fn bom_radar_frames(
        &self,
        ctx: &Context<'_>,
        radar_id: String,
    ) -> Result<impl Stream<Item = RadarFrame>> {
        let location = RadarLocation::load(ctx, &radar_id).await?;
        let seen: Vec<RadarFrame> = RadarFrame::list(ctx, &location.radar_id).await?;
        let bom = bom(ctx).clone();
        let public_url = ctx.data_unchecked::<PublicUrl>().0.clone();
        let prefix = format!("{RADAR_CACHE_PATH}/{}.T.", location.radar_id);

        Ok(async_stream::stream! {
            let mut latest = seen.last().map(|f| f.timestamp.clone());
            let mut ticker = tokio::time::interval(POLL_INTERVAL);
            loop {
                ticker.tick().await;
                let Ok(timestamps) = frame_timestamps(&bom, &prefix, ".png").await else {
                    continue;
                };
                for timestamp in timestamps {
                    if latest.as_ref().is_some_and(|l| &timestamp <= l) {
                        continue;
                    }
                    latest = Some(timestamp.clone());
                    let image = Image::RadarFrame {
                        radar: location.radar_id.clone(),
                        timestamp: timestamp.clone(),
                    };
                    if let Some(captured_at) = parse_timestamp(&timestamp) {
                        yield RadarFrame {
                            radar_id: location.radar_id.clone(),
                            captured_at,
                            url: format!("{public_url}{}", image.path()),
                            timestamp,
                        };
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn subgraph_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subgraph.graphql");
        crate::subgraph::assert_subgraph_current(&super::builder().finish(), path);
    }
}
