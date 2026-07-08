use std::time::Duration;

pub fn current_location(timeout: Duration) -> Result<modem::location::Location, String> {
    current_location_impl(timeout)
}

#[cfg(target_os = "macos")]
fn current_location_impl(timeout: Duration) -> Result<modem::location::Location, String> {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let script = r#"
ObjC.import('CoreLocation');
ObjC.import('Foundation');

const manager = $.CLLocationManager.alloc.init;
if (!manager) {
  throw new Error('Could not create CLLocationManager');
}
if (!$.CLLocationManager.locationServicesEnabled) {
  throw new Error('Location Services disabled');
}

manager.desiredAccuracy = 100.0;
if (typeof manager.requestWhenInUseAuthorization !== 'undefined') {
  manager.requestWhenInUseAuthorization;
}
manager.startUpdatingLocation;

const end = $.NSDate.dateWithTimeIntervalSinceNow(3.0);
let location = manager.location;
while ((!location || typeof location.horizontalAccuracy === 'undefined' || location.horizontalAccuracy < 0) &&
       $.NSDate.date.compare(end) === $.NSOrderedAscending) {
  $.NSRunLoop.currentRunLoop.runUntilDate($.NSDate.dateWithTimeIntervalSinceNow(0.1));
  location = manager.location;
}
manager.stopUpdatingLocation;

if (!location || typeof location.horizontalAccuracy === 'undefined' || location.horizontalAccuracy < 0) {
  throw new Error('No location fix');
}

const coord = location.coordinate;
if (!coord || typeof coord.latitude === 'undefined' || typeof coord.longitude === 'undefined') {
  throw new Error('Location fix did not include coordinates');
}
`${coord.latitude},${coord.longitude},${location.horizontalAccuracy}`;
"#;

    let mut child = Command::new("/usr/bin/osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start macOS location helper: {error}"))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("failed reading location helper output: {error}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    return Err(if stderr.is_empty() {
                        "macOS location helper failed".to_string()
                    } else {
                        stderr
                    });
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                return parse_location_output(stdout.trim());
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("macOS location helper timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(format!("failed polling location helper: {error}")),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn current_location_impl(_timeout: Duration) -> Result<modem::location::Location, String> {
    Err("automatic laptop location is only implemented on macOS".to_string())
}

fn parse_location_output(output: &str) -> Result<modem::location::Location, String> {
    let mut parts = output.split(',').map(str::trim);
    let lat = parts
        .next()
        .ok_or("location helper returned no latitude")?
        .parse::<f64>()
        .map_err(|error| format!("invalid latitude from location helper: {error}"))?;
    let lon = parts
        .next()
        .ok_or("location helper returned no longitude")?
        .parse::<f64>()
        .map_err(|error| format!("invalid longitude from location helper: {error}"))?;
    let accuracy_m = parts
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0);

    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err("location helper returned coordinates outside valid bounds".to_string());
    }

    Ok(modem::location::Location {
        lat,
        lon,
        accuracy_m,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_location_output() {
        let location = parse_location_output("-33.8688,151.2093,42").unwrap();
        assert_eq!(location.lat, -33.8688);
        assert_eq!(location.lon, 151.2093);
        assert_eq!(location.accuracy_m, Some(42.0));
    }

    #[test]
    fn rejects_invalid_location_output() {
        assert!(parse_location_output("-91,151,10").is_err());
        assert!(parse_location_output("nope").is_err());
    }
}
