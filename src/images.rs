use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use regex::Regex;
use std::sync::LazyLock;

use crate::bom::{BOM, RADAR_CACHE_PATH, SATELLITE_CACHE_PATH};

static RADAR_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^IDR\d{3}$").unwrap());
static SATELLITE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^IDE\d{5}$").unwrap());
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{12}$").unwrap());

const FRAME_CACHE: &str = "public, max-age=86400, immutable";
const LATEST_CACHE: &str = "public, max-age=60";

/// A radar or satellite image cached in the bucket, addressed by its public path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    RadarBase {
        radar: String,
    },
    RadarFrame {
        radar: String,
        timestamp: String,
    },
    SatelliteFrame {
        satellite: String,
        timestamp: String,
    },
    SatelliteLatest {
        satellite: String,
    },
}

impl Image {
    pub fn key(&self) -> String {
        match self {
            Self::RadarBase { radar } => format!("{radar}.base.png"),
            Self::RadarFrame { radar, timestamp } => {
                format!("{RADAR_CACHE_PATH}/{radar}.T.{timestamp}.png")
            }
            Self::SatelliteFrame {
                satellite,
                timestamp,
            } => format!("{SATELLITE_CACHE_PATH}/{satellite}.{timestamp}.jpg"),
            Self::SatelliteLatest { satellite } => {
                format!("external/{satellite}.latest.satellite.gif")
            }
        }
    }

    pub fn path(&self) -> String {
        match self {
            Self::RadarBase { radar } => format!("/radar/{radar}/base.png"),
            Self::RadarFrame { radar, timestamp } => format!("/radar/{radar}/{timestamp}.png"),
            Self::SatelliteFrame {
                satellite,
                timestamp,
            } => format!("/satellite/{satellite}/{timestamp}.jpg"),
            Self::SatelliteLatest { satellite } => format!("/satellite/{satellite}/latest.gif"),
        }
    }

    fn content_type(&self) -> &'static str {
        match self {
            Self::RadarBase { .. } | Self::RadarFrame { .. } => "image/png",
            Self::SatelliteFrame { .. } => "image/jpeg",
            Self::SatelliteLatest { .. } => "image/gif",
        }
    }

    fn cache_control(&self) -> &'static str {
        match self {
            Self::SatelliteLatest { .. } => LATEST_CACHE,
            _ => FRAME_CACHE,
        }
    }

    fn radar(radar: &str, file: &str) -> Option<Self> {
        if !RADAR_ID.is_match(radar) {
            return None;
        }
        let radar = radar.to_owned();
        match file {
            "base.png" => Some(Self::RadarBase { radar }),
            _ => {
                let timestamp = file.strip_suffix(".png")?;
                TIMESTAMP.is_match(timestamp).then(|| Self::RadarFrame {
                    radar,
                    timestamp: timestamp.to_owned(),
                })
            }
        }
    }

    fn satellite(satellite: &str, file: &str) -> Option<Self> {
        if !SATELLITE_ID.is_match(satellite) {
            return None;
        }
        let satellite = satellite.to_owned();
        match file {
            "latest.gif" => Some(Self::SatelliteLatest { satellite }),
            _ => {
                let timestamp = file.strip_suffix(".jpg")?;
                TIMESTAMP.is_match(timestamp).then(|| Self::SatelliteFrame {
                    satellite,
                    timestamp: timestamp.to_owned(),
                })
            }
        }
    }
}

pub fn parse_timestamp(timestamp: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(timestamp, "%Y%m%d%H%M")
        .ok()
        .map(|t| t.and_utc())
}

/// Frame timestamps cached for a radar or satellite, oldest first.
pub async fn frame_timestamps(
    bom: &BOM,
    prefix: &str,
    suffix: &str,
) -> anyhow::Result<Vec<String>> {
    let mut timestamps: Vec<String> = bom
        .bucket()
        .list(prefix.to_owned(), None)
        .await?
        .into_iter()
        .flat_map(|page| page.contents)
        .filter_map(|object| {
            let timestamp = object.key.strip_prefix(prefix)?.strip_suffix(suffix)?;
            TIMESTAMP.is_match(timestamp).then(|| timestamp.to_owned())
        })
        .collect();
    timestamps.sort();
    Ok(timestamps)
}

pub fn router(bom: Arc<BOM>) -> Router {
    Router::new()
        .route("/radar/{radar}/{file}", get(radar))
        .route("/satellite/{satellite}/{file}", get(satellite))
        .with_state(bom)
}

async fn radar(
    State(bom): State<Arc<BOM>>,
    Path((radar, file)): Path<(String, String)>,
) -> Response {
    serve(&bom, Image::radar(&radar, &file)).await
}

async fn satellite(
    State(bom): State<Arc<BOM>>,
    Path((satellite, file)): Path<(String, String)>,
) -> Response {
    serve(&bom, Image::satellite(&satellite, &file)).await
}

async fn serve(bom: &BOM, image: Option<Image>) -> Response {
    let Some(image) = image else {
        return StatusCode::NOT_FOUND.into_response();
    };

    match bom.bucket().get_object(image.key()).await {
        Ok(object) if object.status_code() == 200 => (
            [
                (header::CONTENT_TYPE, image.content_type()),
                (header::CACHE_CONTROL, image.cache_control()),
            ],
            object.to_vec(),
        )
            .into_response(),
        Ok(object) if object.status_code() == 404 => StatusCode::NOT_FOUND.into_response(),
        Ok(object) => {
            tracing::warn!(
                key = image.key(),
                status = object.status_code(),
                "bucket read failed"
            );
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(s3::error::S3Error::HttpFailWithBody(404, _)) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!(key = image.key(), "bucket read failed: {e}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_map_to_bucket_keys() {
        let frame = Image::radar("IDR703", "202611061200.png").unwrap();
        assert_eq!(frame.key(), "radar_cache/IDR703.T.202611061200.png");
        assert_eq!(frame.path(), "/radar/IDR703/202611061200.png");
        assert_eq!(
            Image::radar("IDR703", "base.png").unwrap().key(),
            "IDR703.base.png"
        );
        assert_eq!(
            Image::satellite("IDE00416", "latest.gif").unwrap().key(),
            "external/IDE00416.latest.satellite.gif"
        );
        assert_eq!(
            Image::satellite("IDE00416", "202611061200.jpg")
                .unwrap()
                .key(),
            "satellite_cache/IDE00416.202611061200.jpg"
        );
    }

    #[test]
    fn only_known_shapes_are_served() {
        assert!(Image::radar("../secrets", "base.png").is_none());
        assert!(Image::radar("IDR703", "../../x.png").is_none());
        assert!(Image::radar("IDR703", "2026.png").is_none());
        assert!(Image::satellite("IDR703", "latest.gif").is_none());
        assert!(Image::satellite("IDE00416", "latest.png").is_none());
    }
}
