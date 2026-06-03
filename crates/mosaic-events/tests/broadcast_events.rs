#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde round-trip + topic-routing contract tests for the broadcast wire types
//! (Wave A2): alarm lifecycle events carrying [`mosaic_core::alarm::AlarmRecord`],
//! tally state events carrying [`mosaic_core::tally::TallyState`], salvo
//! arm/take events, and the new `alarms`/`tally` topics. These prove the new
//! `Event` variants are internally-tagged (`t`/`data`, never untagged), survive
//! a JSON round-trip, and route to the correct [`Topic`], and that the
//! snapshot ⊕ delta / resume-by-seq ordering still holds for the new topics.

use mosaic_core::alarm::{
    AckState, AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity,
};
use mosaic_core::tally::{Brightness, BusSource, TallyColor, TallyState};
use mosaic_core::time::MediaTime;
use mosaic_events::event::{AlarmTransition, SalvoEvent, SalvoPhase, TallyEvent, TallyTarget};
use mosaic_events::ordering::Accepted;
use mosaic_events::{Envelope, Event, EventEnvelope, FrameKind, Seq, Topic, TopicCursor};
use serde_json::{json, Value};

fn ts() -> MediaTime {
    MediaTime::from_nanos(920_451_123_456)
}

fn sample_record() -> AlarmRecord {
    AlarmRecord::new(
        AlarmId::new("alarm:tile3:black"),
        AlarmKind::Black,
        PerceivedSeverity::Major,
        AlarmScope::Tile { index: 3 },
        ts(),
    )
}

#[test]
fn alarm_topic_routes_and_is_high_rate_excluded() {
    assert_eq!(Topic::Alarms.as_str(), "alarms");
    assert_eq!(Topic::Tally.as_str(), "tally");
    assert!(!Topic::Alarms.is_control());
    assert!(!Topic::Tally.is_control());
    // Neither alarms nor tally are high-rate conflated lanes — they must stay in
    // the lossless replay ring (ADR-RT003).
    assert!(!Topic::Alarms.is_high_rate());
    assert!(!Topic::Tally.is_high_rate());
}

#[test]
fn alarm_topic_roundtrips_on_the_wire() {
    for (topic, wire) in [(Topic::Alarms, "alarms"), (Topic::Tally, "tally")] {
        let v = serde_json::to_value(topic).unwrap();
        assert_eq!(v, json!(wire));
        let back: Topic = serde_json::from_value(v).unwrap();
        assert_eq!(back, topic);
    }
}

#[test]
fn alarm_raised_event_carries_the_core_record() {
    let env: EventEnvelope = Envelope::new(
        Topic::Alarms,
        Seq::new(7001),
        ts(),
        Event::AlarmRaised(AlarmTransition::new(sample_record())),
    )
    .with_id("alarm:tile3:black");

    // Internally-tagged: discriminator flattens to top-level `t`, body under `data`.
    let v: Value = serde_json::to_value(&env).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.get("t").unwrap(), &json!("alarm.raised"));
    assert_eq!(obj.get("topic").unwrap(), &json!("alarms"));
    assert!(
        !obj.contains_key("payload"),
        "Rust field name must not leak"
    );
    let data = obj.get("data").unwrap().as_object().unwrap();
    let record = data.get("record").unwrap().as_object().unwrap();
    assert_eq!(record.get("kind").unwrap(), &json!("Black"));
    assert_eq!(record.get("severity").unwrap(), &json!("Major"));
    // Scope is tagged (`kind`), never untagged.
    let scope = record.get("scope").unwrap().as_object().unwrap();
    assert_eq!(scope.get("kind").unwrap(), &json!("tile"));
    assert_eq!(scope.get("index").unwrap(), &json!(3));

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env, "alarm.raised must survive a JSON round-trip");
}

#[test]
fn all_alarm_lifecycle_variants_roundtrip_and_route() {
    let record = sample_record();
    let acked = {
        let mut r = record.clone();
        r.ack = AckState::acked("operator:jo", ts());
        r
    };
    let cases: Vec<(Event, &str)> = vec![
        (
            Event::AlarmRaised(AlarmTransition::new(record.clone())),
            "alarm.raised",
        ),
        (
            Event::AlarmUpdated(AlarmTransition::new(record.clone())),
            "alarm.updated",
        ),
        (
            Event::AlarmCleared(AlarmTransition::new(record.clone())),
            "alarm.cleared",
        ),
        (
            Event::AlarmAcked(AlarmTransition::new(acked)),
            "alarm.acked",
        ),
    ];
    for (event, tag) in cases {
        assert_eq!(event.type_tag(), tag, "type_tag mismatch for {tag}");
        assert!(!event.is_control(), "{tag} is a data event, not control");
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v.get("t").unwrap(), &json!(tag));
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, event, "{tag} must round-trip");
    }
}

#[test]
fn alarm_acked_event_preserves_ack_state() {
    let mut record = sample_record();
    record.ack = AckState::acked("operator:jo", ts());
    let event = Event::AlarmAcked(AlarmTransition::new(record));
    let back: Event = serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
    match back {
        Event::AlarmAcked(t) => assert!(
            t.record.ack.is_acked(),
            "ack state must round-trip through the alarm.acked event"
        ),
        other => panic!("expected AlarmAcked, got {other:?}"),
    }
}

#[test]
fn tally_state_event_carries_the_core_tally_state() {
    let event = Event::TallyState(TallyEvent {
        target: TallyTarget::Tile { index: 5 },
        state: TallyState {
            color: TallyColor::Red,
            brightness: Brightness::FULL,
            source: BusSource::Program,
        },
    });
    assert_eq!(event.type_tag(), "tally.state");
    assert!(!event.is_control());

    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v.get("t").unwrap(), &json!("tally.state"));
    let data = v.get("data").unwrap().as_object().unwrap();
    // TallyTarget is tagged (`kind`), never untagged.
    let target = data.get("target").unwrap().as_object().unwrap();
    assert_eq!(target.get("kind").unwrap(), &json!("tile"));
    assert_eq!(target.get("index").unwrap(), &json!(5));
    let state = data.get("state").unwrap().as_object().unwrap();
    // BusSource is tagged.
    let source = state.get("source").unwrap().as_object().unwrap();
    assert_eq!(source.get("kind").unwrap(), &json!("program"));

    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event, "tally.state must round-trip");
}

#[test]
fn salvo_lifecycle_variants_roundtrip_and_route() {
    let cases: Vec<(Event, &str, SalvoPhase)> = vec![
        (
            Event::SalvoArmed(SalvoEvent::new("salvo:evening_news", SalvoPhase::Armed)),
            "salvo.armed",
            SalvoPhase::Armed,
        ),
        (
            Event::SalvoTaken(SalvoEvent::new("salvo:evening_news", SalvoPhase::Taken)),
            "salvo.taken",
            SalvoPhase::Taken,
        ),
        (
            Event::SalvoCancelled(SalvoEvent::new("salvo:evening_news", SalvoPhase::Cancelled)),
            "salvo.cancelled",
            SalvoPhase::Cancelled,
        ),
    ];
    for (event, tag, phase) in cases {
        assert_eq!(event.type_tag(), tag);
        assert!(!event.is_control());
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v.get("t").unwrap(), &json!(tag));
        let data = v.get("data").unwrap().as_object().unwrap();
        assert_eq!(data.get("salvo").unwrap(), &json!("salvo:evening_news"));
        // phase is tagged within the body and matches the event.
        let back: Event = serde_json::from_value(v).unwrap();
        match &back {
            Event::SalvoArmed(s) | Event::SalvoTaken(s) | Event::SalvoCancelled(s) => {
                assert_eq!(s.phase, phase);
            }
            other => panic!("expected a salvo event, got {other:?}"),
        }
        assert_eq!(back, event, "{tag} must round-trip");
    }
}

#[test]
fn salvo_head_scope_roundtrips() {
    let event = Event::SalvoTaken(
        SalvoEvent::new("salvo:wall", SalvoPhase::Taken).with_head("head:wall_a"),
    );
    let back: Event = serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
    match back {
        Event::SalvoTaken(s) => assert_eq!(s.head.as_deref(), Some("head:wall_a")),
        other => panic!("expected SalvoTaken, got {other:?}"),
    }
}

#[test]
fn unknown_broadcast_discriminator_is_rejected() {
    // Tagged, never untagged: a near-miss tag must hard-fail, not fall through.
    let bad = json!({"t": "alarm.exploded", "data": {}});
    let parsed: Result<Event, _> = serde_json::from_value(bad);
    assert!(parsed.is_err());
}

#[test]
fn alarms_topic_obeys_snapshot_then_delta_with_resume() {
    // The new topics ride the same lossless snapshot ⊕ delta / resume-by-seq
    // contract: snapshot establishes a baseline, deltas strictly advance, gaps
    // are reported, and a fresh snapshot ($resync) rebuilds.
    let mut cur = TopicCursor::new(Topic::Alarms);
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(7000)).unwrap(),
        Accepted::SnapshotBaseline {
            seq: Seq::new(7000)
        }
    );
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(7001)).unwrap(),
        Accepted::Delta {
            seq: Seq::new(7001)
        }
    );
    // A gap warrants a re-snapshot.
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(7005)).unwrap(),
        Accepted::DeltaWithGap {
            seq: Seq::new(7005),
            gap: 3
        }
    );
    // A non-advancing delta is rejected and never moves the cursor.
    assert!(cur.accept(FrameKind::Delta, Seq::new(7005)).is_err());
    assert_eq!(cur.last_seq(), Some(Seq::new(7005)));
    // Resync rebuilds the baseline (error message carries the wire topic name).
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(1)).unwrap(),
        Accepted::SnapshotBaseline { seq: Seq::new(1) }
    );
}

#[test]
fn tally_topic_ordering_error_names_the_wire_topic() {
    let mut cur = TopicCursor::new(Topic::Tally);
    let err = cur.accept(FrameKind::Delta, Seq::new(1)).unwrap_err();
    match err {
        mosaic_events::Error::NonMonotonic { topic, .. } => assert_eq!(topic, "tally"),
        other => panic!("wrong error: {other:?}"),
    }
}
