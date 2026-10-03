//! Forecasts from the US National Weather Service API (api.weather.gov).
//!
//! US only, which covers Big Bend. The NWS asks every client to identify itself in
//! the User-Agent; set `weather.user_agent` to something with a contact address.

use crate::config::Weather;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::time::Duration;

pub struct Nws {
    agent: ureq::Agent,
    cfg: Weather,
    base: String,
}

impl Nws {
    pub fn new(cfg: &Weather) -> Self {
        Self::with_base(cfg, "https://api.weather.gov")
    }

    pub fn with_base(cfg: &Weather, base: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        Self {
            agent,
            cfg: cfg.clone(),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    fn get(&self, url: &str) -> Result<Value> {
        let mut resp = self
            .agent
            .get(url)
            .header("User-Agent", &self.cfg.user_agent)
            .header("Accept", "application/geo+json")
            .call()
            .with_context(|| format!("GET {url}"))?;
        Ok(resp.body_mut().read_json()?)
    }

    /// Active alerts and the next few forecast periods, abbreviated for CW.
    pub fn forecast(&self, grid: Option<&str>) -> Result<String> {
        let grid = grid.unwrap_or(&self.cfg.default_grid);
        let (lat, lon) = grid_center(grid).with_context(|| format!("bad grid {grid}"))?;
        let point = self.get(&format!("{}/points/{lat:.4},{lon:.4}", self.base))?;
        let Some(url) = point["properties"]["forecast"].as_str() else {
            bail!("no forecast for {grid} (outside NWS coverage?)");
        };
        let fc = self.get(url)?;
        let alerts = self
            .get(&format!(
                "{}/alerts/active?point={lat:.4},{lon:.4}",
                self.base
            ))
            .map(|a| alert_events(&a))
            .unwrap_or_default();
        Ok(format_forecast(grid, &alerts, &fc, self.cfg.periods))
    }
}

/// Centre of a 4- or 6-character Maidenhead locator, as (latitude, longitude).
pub fn grid_center(grid: &str) -> Option<(f64, f64)> {
    if !protocol::is_grid(grid) {
        return None;
    }
    let b: Vec<u8> = grid.to_ascii_uppercase().into_bytes();
    let mut lon = (b[0] - b'A') as f64 * 20.0 - 180.0 + (b[2] - b'0') as f64 * 2.0;
    let mut lat = (b[1] - b'A') as f64 * 10.0 - 90.0 + (b[3] - b'0') as f64;
    if b.len() == 6 {
        lon += (b[4] - b'A') as f64 * (2.0 / 24.0) + 1.0 / 24.0;
        lat += (b[5] - b'A') as f64 * (1.0 / 24.0) + 0.5 / 24.0;
    } else {
        lon += 1.0;
        lat += 0.5;
    }
    Some((lat, lon))
}

fn alert_events(alerts: &Value) -> Vec<String> {
    let mut events: Vec<String> = alerts["features"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["properties"]["event"].as_str().map(str::to_string))
        .collect();
    events.dedup();
    events
}

/// `WX DL89 ALERT RED FLAG WARNING TNGT CLEAR LO 58 WIND W 5 MPH SAT SUNNY HI 97 ...`
pub fn format_forecast(grid: &str, alerts: &[String], fc: &Value, periods: usize) -> String {
    let mut parts = vec![grid.to_string()];
    for a in alerts {
        parts.push(format!("ALERT {a}"));
    }
    for p in fc["properties"]["periods"]
        .as_array()
        .into_iter()
        .flatten()
        .take(periods.max(1))
    {
        let name = p["name"].as_str().unwrap_or("");
        let short = p["shortForecast"].as_str().unwrap_or("");
        let temp = p["temperature"]
            .as_i64()
            .map(|t| t.to_string())
            .unwrap_or_default();
        let hi_lo = if p["isDaytime"].as_bool().unwrap_or(true) {
            "HI"
        } else {
            "LO"
        };
        let wind = format!(
            "{} {}",
            p["windDirection"].as_str().unwrap_or(""),
            p["windSpeed"].as_str().unwrap_or("")
        );
        parts.push(format!(
            "{} {} {hi_lo} {temp} WIND {}",
            abbreviate(name),
            abbreviate(short),
            abbreviate(&wind)
        ));
    }
    parts.join(" ")
}

/// Shorten common forecast words, as weather teletype did.
pub fn abbreviate(text: &str) -> String {
    const TABLE: &[(&str, &str)] = &[
        ("THIS AFTERNOON", "AFTN"),
        ("THIS MORNING", "MRNG"),
        ("TONIGHT", "TNGT"),
        ("TODAY", "TDA"),
        ("OVERNIGHT", "OVNT"),
        ("THUNDERSTORMS", "TSTMS"),
        ("THUNDERSTORM", "TSTM"),
        ("SHOWERS", "SHWRS"),
        ("CHANCE", "CHC"),
        ("SLIGHT", "SLGT"),
        ("LIKELY", "LKLY"),
        ("PARTLY", "PTLY"),
        ("MOSTLY", "MSTLY"),
        ("ISOLATED", "ISOLD"),
        ("SCATTERED", "SCT"),
        ("MONDAY", "MON"),
        ("TUESDAY", "TUE"),
        ("WEDNESDAY", "WED"),
        ("THURSDAY", "THU"),
        ("FRIDAY", "FRI"),
        ("SATURDAY", "SAT"),
        ("SUNDAY", "SUN"),
        (" NIGHT", " NT"),
        (" THEN ", " THN "),
        (" AND ", " "),
    ];
    let mut s = format!(" {} ", text.to_ascii_uppercase());
    for (long, short) in TABLE {
        s = s.replace(long, short);
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_centres() {
        let (lat, lon) = grid_center("DL89").unwrap();
        assert!(
            (lat - 29.5).abs() < 1e-9 && (lon + 103.0).abs() < 1e-9,
            "{lat} {lon}"
        );
        let (lat, lon) = grid_center("FN31pr").unwrap();
        assert!(
            (lat - 41.729).abs() < 0.01 && (lon + 72.708).abs() < 0.01,
            "{lat} {lon}"
        );
        assert!(grid_center("XX99").is_none());
    }

    #[test]
    fn formats_nws_json() {
        let fc: Value = serde_json::from_str(
            r#"{"properties":{"periods":[
                {"name":"Tonight","isDaytime":false,"temperature":58,"windSpeed":"5 mph","windDirection":"W","shortForecast":"Mostly Clear"},
                {"name":"Saturday","isDaytime":true,"temperature":97,"windSpeed":"10 to 15 mph","windDirection":"SW","shortForecast":"Slight Chance Showers And Thunderstorms"},
                {"name":"Saturday Night","isDaytime":false,"temperature":60,"windSpeed":"5 mph","windDirection":"S","shortForecast":"Clear"}]}}"#,
        )
        .unwrap();
        let s = format_forecast("DL89", &["Red Flag Warning".to_string()], &fc, 2);
        assert_eq!(
            s,
            "DL89 ALERT Red Flag Warning TNGT MSTLY CLEAR LO 58 WIND W 5 MPH SAT SLGT CHC SHWRS TSTMS HI 97 WIND SW 10 TO 15 MPH"
        );
    }
}
