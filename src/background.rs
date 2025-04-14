use std::sync::Arc;

use chrono::NaiveDateTime;
use regex::Regex;

use crate::bom;

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
        .list("radar_cache".to_owned(), Some("/".to_owned()))
        .await?
        .into_iter()
        .flat_map(|i| i.contents);

    let match_file_name = Regex::new(r#"^IDR\d{3}\.T\.(?<datetime>\d{12})\.png"#)?;
    let now = chrono::offset::Utc::now().naive_utc();

    for object in objects {
        let name = object.key;
        tracing::info!("checking item: {}", name);
        let Some(caps) = match_file_name.captures(&name) else {
            tracing::info!("no match!");
            continue;
        };

        let datetime = &caps["datetime"];
        // 2025 04 14 12 04
        let datetime = NaiveDateTime::parse_from_str(datetime, "%Y%m%d%H%m")?;
        if (now - datetime).num_hours() > 24 {
            bucket.delete_object(&name).await?;
        }
    }

    Ok(())
}
