//! Geometry crosses between Rust and PostGIS as WKB, converted to and from
//! GeoJSON here. Every spatial question is left to PostGIS.

use geozero::{CoordDimensions, ToJson, ToWkb, geojson::GeoJson, wkb::Wkb};
use serde_json::Value;

/// GeoJSON geometry, as a caller sends it or PostGIS returns it, to WKB.
pub fn to_wkb(geometry: &Value) -> Result<Vec<u8>, String> {
    if !geometry.is_object() {
        return Err("not a GeoJSON geometry object".into());
    }
    GeoJson(&geometry.to_string())
        .to_wkb(CoordDimensions::xy())
        .map_err(|e| format!("not valid GeoJSON geometry: {e}"))
}

/// WKB from PostGIS to a GeoJSON geometry value.
pub fn to_geojson(wkb: &[u8]) -> Result<Value, String> {
    let json = Wkb(wkb).to_json().map_err(|e| format!("unreadable WKB: {e}"))?;
    serde_json::from_str(&json).map_err(|e| format!("unreadable GeoJSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn polygon_survives_a_round_trip() {
        let polygon = json!({"type": "Polygon", "coordinates": [[[0.5, 0.25], [4.5, 0.25], [4.5, 3.75], [0.5, 0.25]]]});
        let back = to_geojson(&to_wkb(&polygon).expect("wkb")).expect("geojson");
        assert_eq!(back, polygon);
    }

    #[test]
    fn non_geometry_is_refused() {
        assert!(to_wkb(&json!("POINT(1 2)")).is_err());
        assert!(to_wkb(&json!({"type": "Polygon", "coordinates": "nope"})).is_err());
    }
}
