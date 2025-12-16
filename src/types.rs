use axum::response::{IntoResponse, Response};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct ForecastForDay {
    pub date_time: String,
    pub code: String,
    pub description: String,
    pub emoji: String,
    pub min: i64,
    pub max: i64,
    pub uv: Option<f64>,
}

#[derive(Serialize, Deserialize)]
pub struct ForecastEndpointResponse {
    pub days: Vec<ForecastForDay>,
}

pub enum AppError {
    Error(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Error(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Something went wrong: {}", e),
            )
                .into_response(),
        }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self::Error(err.into())
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WillyWeatherForecast {
    pub location: Location,
    pub forecasts: Forecasts,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub id: i64,
    pub name: String,
    pub region: String,
    pub state: String,
    pub postcode: String,
    pub time_zone: String,
    pub lat: f64,
    pub lng: f64,
    pub type_id: i64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Forecasts {
    pub weather: Weather,
    pub uv: Uv,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Weather {
    pub days: Vec<WeatherDay>,
    pub units: Units,
    pub issue_date_time: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeatherDay {
    pub date_time: String,
    pub entries: Vec<WeatherEntry>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeatherEntry {
    pub date_time: String,
    pub precis_code: String,
    pub precis: String,
    pub precis_overlay_code: String,
    pub night: bool,
    pub min: i64,
    pub max: i64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Units {
    pub temperature: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Uv {
    pub days: Vec<UvDay>,
    pub issue_date_time: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UvDay {
    pub date_time: String,
    pub entries: Vec<UvEntry>,
    pub alert: Alert,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UvEntry {
    pub date_time: String,
    pub index: f64,
    pub scale: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    pub max_index: f64,
    pub scale: String,
    pub start_date_time: String,
    pub end_date_time: String,
}
