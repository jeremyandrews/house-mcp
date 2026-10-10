//! Drawing units and their length in metres.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A length unit a drawing may be measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LengthUnit {
    Millimetres,
    Centimetres,
    Decimetres,
    Metres,
    Kilometres,
    Inches,
    Feet,
    Yards,
    UsSurveyFeet,
}

impl LengthUnit {
    /// The name a call uses for this unit, as stored with an import.
    pub fn name(self) -> &'static str {
        match self {
            Self::Millimetres => "millimetres",
            Self::Centimetres => "centimetres",
            Self::Decimetres => "decimetres",
            Self::Metres => "metres",
            Self::Kilometres => "kilometres",
            Self::Inches => "inches",
            Self::Feet => "feet",
            Self::Yards => "yards",
            Self::UsSurveyFeet => "us_survey_feet",
        }
    }

    pub fn metres(self) -> f64 {
        match self {
            Self::Millimetres => 0.001,
            Self::Centimetres => 0.01,
            Self::Decimetres => 0.1,
            Self::Metres => 1.0,
            Self::Kilometres => 1000.0,
            Self::Inches => 0.0254,
            Self::Feet => 0.3048,
            Self::Yards => 0.9144,
            Self::UsSurveyFeet => 1200.0 / 3937.0,
        }
    }

    /// The unit named by a DXF `$INSUNITS` header, or `None` when the header
    /// says the drawing is unitless or names a unit no building is drawn in.
    pub fn from_insunits(units: dxf::enums::Units) -> Option<Self> {
        use dxf::enums::Units;
        match units {
            Units::Millimeters => Some(Self::Millimetres),
            Units::Centimeters => Some(Self::Centimetres),
            Units::Decimeters => Some(Self::Decimetres),
            Units::Meters => Some(Self::Metres),
            Units::Kilometers => Some(Self::Kilometres),
            Units::Inches => Some(Self::Inches),
            Units::Feet => Some(Self::Feet),
            Units::Yards => Some(Self::Yards),
            Units::USSurveyFeet => Some(Self::UsSurveyFeet),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dxf::enums::Units;

    #[test]
    fn insunits_map_to_metres() {
        let cases = [
            (Units::Millimeters, 0.001),
            (Units::Centimeters, 0.01),
            (Units::Decimeters, 0.1),
            (Units::Meters, 1.0),
            (Units::Kilometers, 1000.0),
            (Units::Inches, 0.0254),
            (Units::Feet, 0.3048),
            (Units::Yards, 0.9144),
            (Units::USSurveyFeet, 0.304_800_609_6),
        ];
        for (header, metres) in cases {
            let unit = LengthUnit::from_insunits(header).expect("a length unit");
            assert!((unit.metres() - metres).abs() < 1e-9, "{header:?}");
        }
    }

    #[test]
    fn names_match_what_a_call_sends() {
        let all = [
            LengthUnit::Millimetres,
            LengthUnit::Centimetres,
            LengthUnit::Decimetres,
            LengthUnit::Metres,
            LengthUnit::Kilometres,
            LengthUnit::Inches,
            LengthUnit::Feet,
            LengthUnit::Yards,
            LengthUnit::UsSurveyFeet,
        ];
        for unit in all {
            assert_eq!(serde_json::json!(unit), unit.name());
        }
    }

    #[test]
    fn unitless_and_astronomical_headers_have_no_unit() {
        assert_eq!(LengthUnit::from_insunits(Units::Unitless), None);
        assert_eq!(LengthUnit::from_insunits(Units::LightYears), None);
    }
}
