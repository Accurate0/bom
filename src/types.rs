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
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Weather {
    pub days: Vec<Day>,
    pub units: Units,
    pub issue_date_time: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Day {
    pub date_time: String,
    pub entries: Vec<Entry>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
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
