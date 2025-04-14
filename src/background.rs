use std::{path::Path, sync::Arc};

use anyhow::Context;
use chrono::NaiveDateTime;
use regex::Regex;

use crate::bom::{self, RADAR_CACHE_PATH};

pub async fn refresh_all_images(bom: Arc<bom::BOM>) -> Result<(), bom::BOMError> {
    let locations = sqlx::query!("SELECT * FROM locations")
        .fetch_all(bom.db())
        .await?;

    for location in locations {
        tracing::info!("background fetch for {}", location.name);
        bom.fetch_all_images_for(&location.bom_radar_id).await?;
    }

    Ok(())
}

pub async fn cleanup_old_images(bom: Arc<bom::BOM>) -> Result<(), anyhow::Error> {
    let bucket = bom.bucket();
    let objects = bucket
        .list(RADAR_CACHE_PATH.to_owned(), None)
        .await?
        .into_iter()
        .flat_map(|i| i.contents);

    let match_file_name = Regex::new(r#"^IDR\d{3}\.T\.(?<datetime>\d{12})\.png"#)?;
    let now = chrono::offset::Utc::now();

    for object in objects {
        let basename = Path::new(&object.key)
            .file_name()
            .context("invalid filename")?
            .to_str()
            .context("invaid string")?;

        let Some(caps) = match_file_name.captures(basename) else {
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
