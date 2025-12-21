use std::collections::HashSet;

use tracing::instrument;

use crate::types::WillyWeatherForecast;

#[derive(Clone, Debug)]
pub struct WillyWeatherAPI {
    http: reqwest::Client,
    api_key: String,
}

#[derive(thiserror::Error, Debug)]
pub enum WillyWeatherAPIError {
    #[error("a http error occurred: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unknown error occurred: {0}")]
    Unknown(#[from] anyhow::Error),
}

impl WillyWeatherAPI {
    const FORECAST_API_TEMPLATE: &str =
        "https://api.willyweather.com.au/v2/{API_KEY}/locations/{LOCATION_ID}/weather.json";
    pub const PERTH_ID: &str = "14576";

    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::ClientBuilder::new().build().unwrap(),
            api_key,
        }
    }

    pub fn get_locations() -> HashSet<(String, String)> {
        HashSet::from_iter(vec![
            ("Perth".to_owned(), Self::PERTH_ID.to_owned()),
            ("Australind".to_owned(), "15864".to_owned()),
        ])
    }

    #[instrument(skip(self))]
    pub async fn get_forecast(
        &self,
        id: &str,
        days: &i64,
    ) -> Result<WillyWeatherForecast, WillyWeatherAPIError> {
        let url = Self::FORECAST_API_TEMPLATE
            .replace("{API_KEY}", &self.api_key)
            .replace("{LOCATION_ID}", id);

        let response = self
            .http
            .get(url)
            .query(&[("forecasts", "weather,uv"), ("days", &days.to_string())])
            .send()
            .await?
            .error_for_status()?
            .json::<WillyWeatherForecast>()
            .await?;

        Ok(response)
    }
}
