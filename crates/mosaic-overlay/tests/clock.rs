//! Analog + digital clock model: multiple styles, multi-timezone, and an
//! NTP/PTP source-select model that exposes lock / ref-loss status as text+glyph
//! (never colour-only). Formatting and hand angles are checked against known
//! answers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use mosaic_overlay::clock::{
    AnalogHands, ClockFace, ClockModel, RefSource, RefStatus, TimeRef, TimeZoneOffset, WallTime,
};

/// Assert two f32 angles (degrees) are equal within tolerance.
fn assert_deg(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1e-3,
        "expected {expected} deg, got {actual}"
    );
}

#[test]
fn digital_formats_hms_24h() {
    // 13:05:09 into a UTC day.
    let wall = WallTime::from_unix_seconds(13 * 3600 + 5 * 60 + 9);
    let face = ClockFace::digital_24h();
    let local = wall.with_offset(TimeZoneOffset::UTC);
    assert_eq!(face.format(local), "13:05:09");
}

#[test]
fn digital_formats_12h_with_meridiem() {
    let wall = WallTime::from_unix_seconds(13 * 3600 + 5 * 60 + 9);
    let face = ClockFace::digital_12h();
    let local = wall.with_offset(TimeZoneOffset::UTC);
    assert_eq!(face.format(local), "01:05:09 PM");
}

#[test]
fn digital_12h_midnight_is_twelve_am() {
    let wall = WallTime::from_unix_seconds(0);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    assert_eq!(ClockFace::digital_12h().format(local), "12:00:00 AM");
}

#[test]
fn digital_12h_noon_is_twelve_pm() {
    let wall = WallTime::from_unix_seconds(12 * 3600);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    assert_eq!(ClockFace::digital_12h().format(local), "12:00:00 PM");
}

#[test]
fn timezone_offset_shifts_displayed_time() {
    // 00:00:00 UTC seen from +10:00 (e.g. AEST) is 10:00:00.
    let wall = WallTime::from_unix_seconds(0);
    let aest = TimeZoneOffset::from_minutes(10 * 60);
    let local = wall.with_offset(aest);
    assert_eq!(ClockFace::digital_24h().format(local), "10:00:00");
}

#[test]
fn negative_timezone_offset_wraps_to_previous_day() {
    // 00:30:00 UTC seen from -05:00 is 19:30:00 (previous day wall clock).
    let wall = WallTime::from_unix_seconds(30 * 60);
    let est = TimeZoneOffset::from_minutes(-5 * 60);
    let local = wall.with_offset(est);
    assert_eq!(ClockFace::digital_24h().format(local), "19:30:00");
}

#[test]
fn analog_hands_at_three_oclock() {
    // 03:00:00 — hour hand at 90 deg, minute and second at 0.
    let wall = WallTime::from_unix_seconds(3 * 3600);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    let hands = AnalogHands::from(local);
    assert_deg(hands.hour_deg, 90.0);
    assert_deg(hands.minute_deg, 0.0);
    assert_deg(hands.second_deg, 0.0);
}

#[test]
fn analog_hour_hand_advances_with_minutes() {
    // 01:30:00 — hour hand halfway between 1 and 2 = 45 deg; minute at 180.
    let wall = WallTime::from_unix_seconds(3600 + 30 * 60);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    let hands = AnalogHands::from(local);
    assert_deg(hands.hour_deg, 45.0);
    assert_deg(hands.minute_deg, 180.0);
    assert_deg(hands.second_deg, 0.0);
}

#[test]
fn analog_second_hand_at_quarter() {
    // ...:...:15 — second hand at 90 deg.
    let wall = WallTime::from_unix_seconds(15);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    let hands = AnalogHands::from(local);
    assert_deg(hands.second_deg, 90.0);
}

#[test]
fn ref_status_is_text_and_glyph_not_colour() {
    // a11y: lock/ref-loss status must be conveyed as text + glyph.
    assert_eq!(RefStatus::Locked.label(), "locked");
    assert_eq!(RefStatus::Holdover.label(), "holdover");
    assert_eq!(RefStatus::RefLoss.label(), "ref-loss");
    assert_eq!(RefStatus::Freerun.label(), "freerun");
    // Distinct glyphs so the status reads without colour.
    let glyphs = [
        RefStatus::Locked.glyph(),
        RefStatus::Holdover.glyph(),
        RefStatus::RefLoss.glyph(),
        RefStatus::Freerun.glyph(),
    ];
    for (i, a) in glyphs.iter().enumerate() {
        for b in glyphs.iter().skip(i + 1) {
            assert_ne!(a, b, "ref-status glyphs must be distinct");
        }
    }
}

#[test]
fn ref_loss_is_disciplined_only_when_locked_or_holdover() {
    assert!(RefStatus::Locked.is_disciplined());
    assert!(RefStatus::Holdover.is_disciplined());
    assert!(!RefStatus::RefLoss.is_disciplined());
    assert!(!RefStatus::Freerun.is_disciplined());
}

#[test]
fn source_select_carries_kind_and_status() {
    let r = TimeRef::new(RefSource::Ptp, RefStatus::Locked);
    assert_eq!(r.source, RefSource::Ptp);
    assert_eq!(r.status, RefStatus::Locked);
    // Display string conveys both source and status as text.
    assert_eq!(r.status_text(), "PTP locked");
    let ntp = TimeRef::new(RefSource::Ntp, RefStatus::RefLoss);
    assert_eq!(ntp.status_text(), "NTP ref-loss");
}

#[test]
fn clock_model_carries_face_zone_and_ref() {
    let model = ClockModel::new(
        ClockFace::digital_24h(),
        TimeZoneOffset::from_minutes(0),
        TimeRef::new(RefSource::Ntp, RefStatus::Locked),
    );
    let wall = WallTime::from_unix_seconds(3661); // 01:01:01
    assert_eq!(model.render_digital(wall), Some("01:01:01".to_owned()));
    assert!(model.render_analog(wall).is_none());
    // The ref status is exposed for the a11y badge.
    assert_eq!(model.time_ref.status_text(), "NTP locked");
}

#[test]
fn clock_model_round_trips_through_json() {
    let model = ClockModel::new(
        ClockFace::analog(),
        TimeZoneOffset::from_minutes(-330),
        TimeRef::new(RefSource::Ptp, RefStatus::Holdover),
    );
    let json = serde_json::to_string(&model).unwrap();
    let back: ClockModel = serde_json::from_str(&json).unwrap();
    assert_eq!(model, back);
}
