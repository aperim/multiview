//! Tests for the SNMP trap / MIB type scaffold.
//!
//! Gated on the `snmp` Cargo feature: the module only compiles when that
//! off-by-default feature is enabled, so the whole file is a no-op otherwise.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![cfg(feature = "snmp")]

use multiview_core::alarm::PerceivedSeverity;
use multiview_telemetry::snmp::{
    severity_index, Oid, SnmpError, Trap, VarBind, VarBindValue, ENTERPRISES_ARC,
};
use serde_json::json;

#[test]
fn oid_rejects_empty_arc() {
    let err = Oid::new(Vec::<u32>::new()).unwrap_err();
    assert!(matches!(err, SnmpError::EmptyOid));
}

#[test]
fn oid_renders_dotted_decimal() {
    let oid = Oid::new(ENTERPRISES_ARC.to_vec()).unwrap();
    assert_eq!(oid.to_dotted(), "1.3.6.1.4.1");
    assert_eq!(oid.arc(), ENTERPRISES_ARC);
}

#[test]
fn oid_child_appends_subidentifiers() {
    let root = Oid::new(ENTERPRISES_ARC.to_vec()).unwrap();
    let child = root.child([99999, 1, 2]);
    assert_eq!(child.to_dotted(), "1.3.6.1.4.1.99999.1.2");
    // Parent is unchanged.
    assert_eq!(root.to_dotted(), "1.3.6.1.4.1");
}

#[test]
fn severity_index_is_strictly_monotonic_over_x733() {
    let ladder = [
        PerceivedSeverity::Cleared,
        PerceivedSeverity::Indeterminate,
        PerceivedSeverity::Warning,
        PerceivedSeverity::Minor,
        PerceivedSeverity::Major,
        PerceivedSeverity::Critical,
    ];
    for window in ladder.windows(2) {
        assert!(
            severity_index(window[1]) > severity_index(window[0]),
            "{:?} must out-rank {:?}",
            window[1],
            window[0],
        );
    }
    assert_eq!(severity_index(PerceivedSeverity::Cleared), 1);
    assert_eq!(severity_index(PerceivedSeverity::Critical), 6);
}

#[test]
fn varbind_value_serialises_tagged() {
    let value = VarBindValue::Integer { value: -7 };
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        json!({ "type": "integer", "value": -7 })
    );

    let octet = VarBindValue::OctetString {
        value: "tile-3 black".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(&octet).unwrap(),
        json!({ "type": "octet_string", "value": "tile-3 black" })
    );
}

#[test]
fn trap_roundtrips_through_serde() {
    let trap_oid = Oid::new(ENTERPRISES_ARC.to_vec())
        .unwrap()
        .child([99999, 0, 1]);
    let severity_oid = Oid::new(ENTERPRISES_ARC.to_vec())
        .unwrap()
        .child([99999, 1, 1]);
    let trap = Trap::new(
        4242,
        trap_oid,
        vec![VarBind::new(
            severity_oid,
            VarBindValue::Integer {
                value: severity_index(PerceivedSeverity::Major),
            },
        )],
    );
    let json = serde_json::to_string(&trap).unwrap();
    let back: Trap = serde_json::from_str(&json).unwrap();
    assert_eq!(trap, back);
    assert_eq!(back.sys_up_time, 4242);
    assert_eq!(back.bindings.len(), 1);
}
