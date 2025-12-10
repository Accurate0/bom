use serde::{Deserialize, Serialize};

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
