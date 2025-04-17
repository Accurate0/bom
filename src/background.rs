use std::{path::Path, sync::Arc};

use anyhow::Context;
use chrono::NaiveDateTime;
use regex::Regex;

use crate::bom::{self, RADAR_CACHE_PATH, SATELLITE_CACHE_PATH};

pub async fn refresh_all_images(bom: Arc<bom::BOM>) -> Result<(), bom::BOMError> {
    let locations = sqlx::query!("SELECT * FROM locations")
        .fetch_all(bom.db())
        .await?;

    for location in locations {
        tracing::info!("background fetch for {}", location.name);
        if let Err(e) = bom.fetch_all_radar_images_for(&location.bom_radar_id).await {
            tracing::error!("radar image failed: {e}");
        };

        tracing::info!("generating timelapse for {}", location.name);
        if let Err(e) = bom
            .generate_radar_timelapse_24hr_for(&location.bom_radar_id)
            .await
        {
            tracing::error!("radar timelapse failed: {e}")
        };
    }

    let satellites = sqlx::query!("SELECT * FROM satellites")
        .fetch_all(bom.db())
        .await?;

    for location in satellites {
        tracing::info!("background fetch for {}", location.name);
        if let Err(e) = bom
            .fetch_all_satellite_images_for(&location.bom_satellite_id)
            .await
        {
            tracing::error!("error in satellite fetch: {e}");
        };

        tracing::info!("updating latest satellite gif for {}", location.name);
        if let Err(e) = bom
            .generate_satellite_gif_for(&location.bom_satellite_id)
            .await
        {
            tracing::error!("error encoding gif: {e}");
        }
    }

    Ok(())
}

pub async fn cleanup_old_images(bom: Arc<bom::BOM>) -> Result<(), anyhow::Error> {
    let bucket = bom.bucket();
    let radar_objects = bucket
        .list(RADAR_CACHE_PATH.to_owned(), None)
        .await?
        .into_iter()
        .flat_map(|i| i.contents);

    let match_radar_filename = Regex::new(r#"^IDR\d{3}\.T\.(?<datetime>\d{12})\.png"#)?;
    let match_satellite_filename = Regex::new(r#"^IDE\d{5}\.(?<datetime>\d{12})\.jpg"#)?;
    let now = chrono::offset::Utc::now();

    for object in radar_objects {
        let basename = Path::new(&object.key)
            .file_name()
            .context("invalid filename")?
            .to_str()
            .context("invalid string")?;

        let Some(caps) = match_radar_filename.captures(basename) else {
            tracing::info!("item: {basename} no match");
            continue;
        };

        let datetime = &caps["datetime"];
        // 2025 04 14 12 04
        let datetime = NaiveDateTime::parse_from_str(datetime, "%Y%m%d%H%M")?.and_utc();
        let difference_in_hours = (now - datetime).num_hours();
        if difference_in_hours > 24 {
            bucket.delete_object(&object.key).await?;
        }

        tracing::info!("item: {basename} matched {datetime} {difference_in_hours}");
    }

    let satellite_objects = bucket
        .list(SATELLITE_CACHE_PATH.to_owned(), None)
        .await?
        .into_iter()
        .flat_map(|i| i.contents);

    for object in satellite_objects {
        let basename = Path::new(&object.key)
            .file_name()
            .context("invalid filename")?
            .to_str()
            .context("invalid string")?;

        let Some(caps) = match_satellite_filename.captures(basename) else {
            tracing::info!("item: {basename} no match");
            continue;
        };

        let datetime = &caps["datetime"];
        // 2025 04 14 12 04
        let datetime = NaiveDateTime::parse_from_str(datetime, "%Y%m%d%H%M")?.and_utc();
        let difference_in_hours = (now - datetime).num_hours();
        if difference_in_hours > 24 {
            bucket.delete_object(&object.key).await?;
        }

        tracing::info!("item: {basename} matched {datetime} {difference_in_hours}");
    }

    Ok(())
}
