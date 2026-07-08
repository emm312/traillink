#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocationMessage {
    pub location: Location,
    pub message: Option<String>,
}

pub fn format_location(location: Location) -> String {
    let accuracy = location
        .accuracy_m
        .map(|value| format!("{:.0}", value.max(0.0)))
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "LOC:{:.6},{:.6};ACC:{}",
        location.lat, location.lon, accuracy
    )
}

pub fn format_location_message(location: Location, message: &str) -> String {
    let message = message.trim();
    if message.is_empty() {
        format_location(location)
    } else {
        format!("{};MSG:{}", format_location(location), message)
    }
}

pub fn parse_location_message(text: &str) -> Option<LocationMessage> {
    let loc_start = text.find("LOC:")? + "LOC:".len();
    let after_loc = &text[loc_start..];
    let loc_end = after_loc.find(';').unwrap_or(after_loc.len());
    let coords = &after_loc[..loc_end];
    let (lat_s, lon_s) = coords.split_once(',')?;
    let lat = lat_s.trim().parse::<f64>().ok()?;
    let lon = lon_s.trim().parse::<f64>().ok()?;

    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }

    let mut accuracy_m = None;
    let mut message = None;
    for part in after_loc[loc_end..].split(';') {
        if let Some(value) = part.strip_prefix("ACC:") {
            let value = value.trim();
            if value != "unknown" {
                accuracy_m = value.parse::<f64>().ok().filter(|v| *v >= 0.0);
            }
        } else if let Some(value) = part.strip_prefix("MSG:") {
            let value = value.trim();
            if !value.is_empty() {
                message = Some(value.to_string());
            }
        }
    }

    Some(LocationMessage {
        location: Location {
            lat,
            lon,
            accuracy_m,
        },
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_location_message() {
        let location = Location {
            lat: -33.8688,
            lon: 151.2093,
            accuracy_m: Some(12.4),
        };
        let text = format_location_message(location, "Need weather");
        assert_eq!(text, "LOC:-33.868800,151.209300;ACC:12;MSG:Need weather");

        let parsed = parse_location_message(&text).unwrap();
        assert_eq!(parsed.location.lat, -33.8688);
        assert_eq!(parsed.location.lon, 151.2093);
        assert_eq!(parsed.location.accuracy_m, Some(12.0));
        assert_eq!(parsed.message.as_deref(), Some("Need weather"));
    }

    #[test]
    fn rejects_invalid_coordinates() {
        assert!(parse_location_message("LOC:-91.0,151.0;ACC:unknown").is_none());
        assert!(parse_location_message("LOC:-33.0,181.0;ACC:unknown").is_none());
    }
}
