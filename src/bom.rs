use async_ftp::{FtpError, FtpStream};
use image::{codecs::gif::GifEncoder, imageops, Delay, DynamicImage, GenericImageView};
use s3::error::S3Error;
use sqlx::PgPool;
use std::path::Path;
use tokio::io::AsyncReadExt;

#[allow(clippy::upper_case_acronyms)]
pub struct BOM {
    bucket: Box<s3::Bucket>,
    db: PgPool,
}

const FILE_TYPES_TO_MERGE: [&str; 4] = ["background", "topography", "locations", "range"];
const RADAR_BACKGROUND_PATH: &str = "/anon/gen/radar_transparencies";
const RADAR_DATA_PATH: &str = "/anon/gen/radar";
const SATELLITE_DATA_PATH: &str = "/anon/gen/gms";
pub const RADAR_CACHE_PATH: &str = "radar_cache";
pub const SATELLITE_CACHE_PATH: &str = "satellite_cache";

const IMAGE_HOST: &str = "https://bom-images.anurag.sh";

#[derive(thiserror::Error, Debug)]
pub enum BOMError {
    #[error("an unspecified internal error occurred: {0}")]
    Unknown(#[from] anyhow::Error),

    #[error("a ftp response error occurred: {0}")]
    Ftp(#[from] async_ftp::FtpError),

    #[error("a sql error occurred: {0}")]
    Sql(#[from] sqlx::Error),

    #[error("a image error occurred: {0}")]
    Image(#[from] image::ImageError),

    #[error("an io error occurred: {0}")]
    Io(#[from] std::io::Error),

    #[error("a s3 error occurred: {0}")]
    S3(#[from] S3Error),

    #[error("a jpg compression error occurred: {0}")]
    JpgCompression(#[from] turbojpeg::Error),
}

impl BOM {
    async fn get_ftp_client_session() -> Result<FtpStream, FtpError> {
        let addr = ("ftp.bom.gov.au", 21);
        let mut ftp_client = FtpStream::connect(addr).await?;
        ftp_client.login("anonymous", "anonymous").await?;
        Ok(ftp_client)
    }

    pub async fn new(bucket: Box<s3::Bucket>, db: PgPool) -> Result<Self, BOMError> {
        Ok(Self { bucket, db })
    }

    pub fn db(&self) -> &PgPool {
        &self.db
    }

    pub fn bucket(&self) -> &s3::Bucket {
        &self.bucket
    }

    pub async fn generate_radar_backgrounds(&self) -> Result<(), BOMError> {
        let locations = sqlx::query!("SELECT * FROM locations")
            .fetch_all(&self.db)
            .await?;

        tracing::info!("pre-generating radar backgrounds");

        let mut ftp_client = Self::get_ftp_client_session().await?;
        for location in locations {
            tracing::info!("generating background for {}", location.name);
            let bom_id = location.bom_radar_id;
            let mut files = Vec::with_capacity(FILE_TYPES_TO_MERGE.len());

            for file_type in FILE_TYPES_TO_MERGE {
                let file_to_fetch = format!("{RADAR_BACKGROUND_PATH}/{bom_id}.{file_type}.png");
                tracing::info!("fetching {file_to_fetch}");
                let img = self
                    .get_or_fetch_image(
                        RADAR_CACHE_PATH,
                        &file_to_fetch,
                        "image/png",
                        &mut ftp_client,
                    )
                    .await?;
                files.push(img);
            }

            // get rain legend (our base image)
            let file_to_fetch = format!("{RADAR_BACKGROUND_PATH}/IDR.legend.0.png");
            tracing::info!("fetching {file_to_fetch}");
            let mut rain_legend = self
                .get_or_fetch_image(
                    RADAR_CACHE_PATH,
                    &file_to_fetch,
                    "image/png",
                    &mut ftp_client,
                )
                .await?;

            for top in files {
                imageops::overlay(&mut rain_legend, &top, 0, 0);
            }

            let mut bytes = Vec::new();
            rain_legend.write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )?;

            let path = format!("{}.base.png", bom_id);
            self.bucket
                .put_object_with_content_type(path, bytes.as_ref(), "image/png")
                .await?;
        }

        Ok(())
    }

    async fn fetch_image(
        &self,
        path: &str,
        mime: &str,
        ftp_client: &mut FtpStream,
    ) -> Result<(), BOMError> {
        let path_obj = Path::new(path);
        let basename = path_obj.file_name().unwrap().to_str().unwrap();

        let cache_path = format!("{RADAR_CACHE_PATH}/{basename}");
        let head_result = self.bucket.head_object(&cache_path).await;
        let is_missing = head_result.is_err();

        if is_missing {
            tracing::info!("downloading {path}");
            let mut buffer = Vec::new();
            let _size = ftp_client
                .simple_retr(path)
                .await?
                .read_to_end(&mut buffer)
                .await?;

            self.bucket
                .put_object_with_content_type(cache_path, &buffer, mime)
                .await?;
        }

        Ok(())
    }

    async fn get_or_fetch_compressed_resized(
        &self,
        path: &str,
        mime: &str,
        ftp_client: &mut FtpStream,
    ) -> Result<DynamicImage, BOMError> {
        let path_obj = Path::new(path);
        let basename = path_obj.file_name().unwrap().to_str().unwrap();

        let cache_path = format!("{SATELLITE_CACHE_PATH}/{basename}");
        let head_result = self.bucket.head_object(&cache_path).await;
        let is_missing = head_result.is_err();

        if is_missing {
            tracing::info!("downloading {path}");
            let mut buffer = Vec::new();
            let _size = ftp_client
                .simple_retr(path)
                .await?
                .read_to_end(&mut buffer)
                .await?;

            let img = image::ImageReader::new(std::io::Cursor::new(buffer))
                .with_guessed_format()?
                .decode()?;

            let bytes = Self::compress_jpg(img)?;

            self.bucket
                .put_object_with_content_type(cache_path, &bytes, mime)
                .await?;

            Ok(image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()?
                .decode()?)
        } else {
            tracing::info!("already exists in s3 {path}");
            let file = self.bucket.get_object(cache_path).await?;
            let file = file.into_bytes().to_vec();

            let img = image::ImageReader::new(std::io::Cursor::new(file))
                .with_guessed_format()?
                .decode()?;

            Ok(img)
        }
    }

    fn compress_jpg(img: DynamicImage) -> Result<Vec<u8>, BOMError> {
        let img = img.resize(300, 300, imageops::FilterType::Gaussian);

        let (width, height) = img.dimensions();
        let format = turbojpeg::PixelFormat::RGB;
        let image = turbojpeg::Image {
            pixels: img.as_bytes(),
            width: width as usize,
            pitch: format.size() * width as usize,
            height: height as usize,
            format,
        };

        let mut compressor = turbojpeg::Compressor::new()?;
        compressor.set_quality(75)?;
        compressor.set_subsamp(turbojpeg::Subsamp::Sub2x1)?;
        compressor.compress_to_vec(image).map_err(|e| e.into())
    }

    async fn fetch_compressed_and_resized(
        &self,
        path: &str,
        mime: &str,
        ftp_client: &mut FtpStream,
    ) -> Result<(), BOMError> {
        let path_obj = Path::new(path);
        let basename = path_obj.file_name().unwrap().to_str().unwrap();

        let cache_path = format!("{SATELLITE_CACHE_PATH}/{basename}");
        let head_result = self.bucket.head_object(&cache_path).await;
        let is_missing = head_result.is_err();

        if is_missing {
            tracing::info!("downloading {path}");
            let mut buffer = Vec::new();
            let _size = ftp_client
                .simple_retr(path)
                .await?
                .read_to_end(&mut buffer)
                .await?;

            let img = image::ImageReader::new(std::io::Cursor::new(buffer))
                .with_guessed_format()?
                .decode()?;

            let bytes = Self::compress_jpg(img)?;

            self.bucket
                .put_object_with_content_type(cache_path, &bytes, mime)
                .await?;
        }

        Ok(())
    }

    async fn get_or_fetch_image(
        &self,
        cache_path: &str,
        path: &str,
        mime: &str,
        ftp_client: &mut FtpStream,
    ) -> Result<DynamicImage, BOMError> {
        let path_obj = Path::new(path);
        let basename = path_obj.file_name().unwrap().to_str().unwrap();

        let cache_path = format!("{cache_path}/{basename}");
        let head_result = self.bucket.head_object(&cache_path).await;
        let is_missing = head_result.is_err();

        if is_missing {
            tracing::info!("downloading {path}");
            let mut buffer = Vec::new();
            let _size = ftp_client
                .simple_retr(path)
                .await?
                .read_to_end(&mut buffer)
                .await?;

            self.bucket
                .put_object_with_content_type(cache_path, &buffer, mime)
                .await?;

            let img = image::ImageReader::new(std::io::Cursor::new(buffer))
                .with_guessed_format()?
                .decode()?;

            Ok(img)
        } else {
            tracing::info!("already exists in s3 {path}");
            let file = self.bucket.get_object(cache_path).await?;
            let file = file.into_bytes().to_vec();

            let img = image::ImageReader::new(std::io::Cursor::new(file))
                .with_guessed_format()?
                .decode()?;

            Ok(img)
        }
    }

    pub async fn fetch_all_radar_images_for(&self, bom_id: &str) -> Result<(), BOMError> {
        let mut ftp_client = Self::get_ftp_client_session().await?;
        let radar_images = ftp_client
            .nlst(Some(RADAR_DATA_PATH))
            .await?
            .into_iter()
            .filter(|i| i.starts_with(&format!("{RADAR_DATA_PATH}/{bom_id}")))
            .filter(|i| i.ends_with(".png"));

        for file in radar_images {
            self.fetch_image(&file, "image/png", &mut ftp_client)
                .await?;
        }

        Ok(())
    }

    pub async fn fetch_all_satellite_images_for(&self, bom_id: &str) -> Result<(), BOMError> {
        let mut ftp_client = Self::get_ftp_client_session().await?;
        let satellite_images = ftp_client
            .nlst(Some(SATELLITE_DATA_PATH))
            .await?
            .into_iter()
            .filter(|i| i.starts_with(&format!("{SATELLITE_DATA_PATH}/{bom_id}")))
            .filter(|i| i.ends_with(".jpg"));

        for file in satellite_images {
            self.fetch_compressed_and_resized(&file, "image/jpg", &mut ftp_client)
                .await?;
        }

        Ok(())
    }

    pub async fn generate_satellite_gif_for(&self, bom_id: &str) -> Result<String, BOMError> {
        let now = chrono::offset::Utc::now().naive_utc();
        let datetime = now.format("%Y%m%d%H%M").to_string();
        let bucket_path = format!("external/{}.{datetime}.satellite.gif", bom_id);

        let existing_obj = self.bucket.head_object(&bucket_path).await;
        if existing_obj.is_ok() {
            return Ok(format!("{IMAGE_HOST}/{bucket_path}"));
        }

        let mut ftp_client = Self::get_ftp_client_session().await?;
        let mut satellite_images = ftp_client
            .nlst(Some(SATELLITE_DATA_PATH))
            .await?
            .into_iter()
            .filter(|i| i.starts_with(&format!("{SATELLITE_DATA_PATH}/{bom_id}")))
            .filter(|i| i.ends_with(".jpg"))
            .collect::<Vec<_>>();

        satellite_images.sort();

        let mut final_gif = Vec::<u8>::new();
        let mut final_gif_cursor = std::io::Cursor::new(&mut final_gif);
        let mut gif_encoder = GifEncoder::new_with_speed(&mut final_gif_cursor, 15);
        gif_encoder.set_repeat(image::codecs::gif::Repeat::Infinite)?;

        let mut images = Vec::new();
        for file in satellite_images.iter().rev().take(30).rev() {
            let img = self
                .get_or_fetch_compressed_resized(file, "image/jpg", &mut ftp_client)
                .await?;

            images.push(img);
        }

        tracing::info!("generating frames for satellite");
        let frames = images.into_iter().map(|i| {
            image::Frame::from_parts(i.to_rgba8(), 0, 0, Delay::from_numer_denom_ms(215, 1))
        });

        tracing::info!("encoding gif for satellite");
        gif_encoder.encode_frames(frames)?;
        drop(gif_encoder);

        tracing::info!("final gif size: {}", final_gif.len());

        // ok?

        self.bucket
            .put_object_with_content_type(&bucket_path, &final_gif, "image/gif")
            .await?;

        Ok(format!("{IMAGE_HOST}/{bucket_path}"))
    }

    pub async fn generate_radar_gif_for(&self, bom_id: &str) -> Result<String, BOMError> {
        let now = chrono::offset::Utc::now().naive_utc();
        let datetime = now.format("%Y%m%d%H%M").to_string();
        let bucket_path = format!("external/{}.{datetime}.radar.gif", bom_id);

        let existing_obj = self.bucket.head_object(&bucket_path).await;
        if existing_obj.is_ok() {
            return Ok(format!("{IMAGE_HOST}/{bucket_path}"));
        }

        let mut ftp_client = Self::get_ftp_client_session().await?;
        let mut radar_images = ftp_client
            .nlst(Some(RADAR_DATA_PATH))
            .await?
            .into_iter()
            .filter(|i| i.starts_with(&format!("{RADAR_DATA_PATH}/{bom_id}")))
            .filter(|i| i.ends_with(".png"))
            .collect::<Vec<_>>();

        radar_images.sort();

        let mut final_gif = Vec::<u8>::new();
        let mut final_gif_cursor = std::io::Cursor::new(&mut final_gif);
        let mut gif_encoder = GifEncoder::new_with_speed(&mut final_gif_cursor, 1);
        gif_encoder.set_repeat(image::codecs::gif::Repeat::Infinite)?;

        let base_image = self.bucket.get_object(format!("{bom_id}.base.png")).await?;
        let base_image_bytes = base_image.into_bytes().to_vec();
        let base_image = image::ImageReader::new(std::io::Cursor::new(base_image_bytes))
            .with_guessed_format()?
            .decode()?;

        let mut images = Vec::new();
        for file in radar_images.iter().rev().take(7).rev() {
            let mut base_image_clone = base_image.clone();

            let img = self
                .get_or_fetch_image(RADAR_CACHE_PATH, file, "image/png", &mut ftp_client)
                .await?;

            imageops::overlay(&mut base_image_clone, &img, 0, 0);
            images.push(base_image_clone);
        }

        let frames = images.into_iter().map(|i| {
            image::Frame::from_parts(i.to_rgba8(), 0, 0, Delay::from_numer_denom_ms(350, 1))
        });
        gif_encoder.encode_frames(frames)?;

        // ok?
        drop(gif_encoder);

        self.bucket
            .put_object_with_content_type(&bucket_path, &final_gif, "image/gif")
            .await?;

        Ok(format!("{IMAGE_HOST}/{bucket_path}"))
    }
}
