//! Typed validation of source/output/overlay resource bodies (ADR-W015).
//!
//! The resource stores keep bodies as opaque JSON so optional fields round-trip
//! losslessly, but the API boundary refuses documents the engine could never
//! run: every `POST`/`PUT` body must deserialize against the canonical
//! `multiview_config` type for its collection. Failures become
//! `422 /problems/validation` with the offending field path
//! (`serde_path_to_error`), so a client knows exactly which field to fix.
//!
//! The resource id is part of the contract: a body may omit `id` (the path id
//! is injected) but a present, different `id` is rejected — one document, one
//! address.
use axum::http::{header::HeaderName, HeaderValue};
use axum::response::Response;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::ControlError;

/// The response header declaring how a stored mutation takes effect
/// (ADR-W015 §4). `restart` until live resource apply lands; the value then
/// flips per-class without a path change.
pub(crate) const APPLY_HEADER: HeaderName = HeaderName::from_static("x-multiview-apply");

/// Mark a mutation response as taking effect via config export + restart.
pub(crate) fn with_apply_restart(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(APPLY_HEADER, HeaderValue::from_static("restart"));
    response
}

/// The collections this module validates, naming the target config type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypedCollection {
    /// `multiview_config::Source`.
    Sources,
    /// `multiview_config::Output`.
    Outputs,
    /// `multiview_config::Overlay`.
    Overlays,
}

impl TypedCollection {
    /// The collection name used in validation messages.
    fn name(self) -> &'static str {
        match self {
            Self::Sources => "source",
            Self::Outputs => "output",
            Self::Overlays => "overlay",
        }
    }
}

/// Validate `body` against the collection's config type, returning the body to
/// store (with the path `id` injected when the body omitted it).
///
/// # Errors
///
/// [`ControlError::Validation`] when the body is not an object, carries an `id`
/// different from the path id, or fails typed deserialization — the detail
/// names the field path and the serde message.
pub(crate) fn validated_body(
    collection: TypedCollection,
    id: &str,
    body: &Value,
) -> Result<Value, ControlError> {
    let Some(map) = body.as_object() else {
        return Err(ControlError::Validation(format!(
            "{} body must be a JSON object",
            collection.name()
        )));
    };
    let mut map = map.clone();
    match map.get("id").and_then(Value::as_str) {
        Some(body_id) if body_id != id => {
            return Err(ControlError::Validation(format!(
                "{} body id {body_id:?} does not match the resource id {id:?}",
                collection.name()
            )));
        }
        Some(_) => {}
        None => {
            if map.contains_key("id") {
                return Err(ControlError::Validation(format!(
                    "{} body `id` must be a string",
                    collection.name()
                )));
            }
            map.insert("id".to_owned(), Value::String(id.to_owned()));
        }
    }
    let candidate = Value::Object(map);
    match collection {
        TypedCollection::Sources => typecheck::<multiview_config::Source>(collection, &candidate)?,
        TypedCollection::Outputs => typecheck::<multiview_config::Output>(collection, &candidate)?,
        TypedCollection::Overlays => {
            typecheck::<multiview_config::Overlay>(collection, &candidate)?;
        }
    }
    Ok(candidate)
}

/// Deserialize `candidate` as `T`, mapping failure to a path-qualified
/// validation error.
fn typecheck<T: DeserializeOwned>(
    collection: TypedCollection,
    candidate: &Value,
) -> Result<(), ControlError> {
    let deserializer = candidate.clone();
    serde_path_to_error::deserialize::<_, T>(deserializer)
        .map(|_| ())
        .map_err(|err| {
            let path = err.path().to_string();
            let message = err.into_inner().to_string();
            ControlError::Validation(if path == "." {
                format!("{} body invalid: {message}", collection.name())
            } else {
                format!("{} body invalid at `{path}`: {message}", collection.name())
            })
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::json;

    use super::{validated_body, TypedCollection};

    #[test]
    fn injects_the_path_id_when_absent() {
        let body = json!({ "kind": "bars" });
        let stored = validated_body(TypedCollection::Sources, "cam1", &body).unwrap();
        assert_eq!(stored["id"], "cam1");
    }

    #[test]
    fn rejects_a_mismatched_id() {
        let body = json!({ "id": "other", "kind": "bars" });
        let err = validated_body(TypedCollection::Sources, "cam1", &body).unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn rejects_an_unknown_source_kind_naming_the_variant() {
        let body = json!({ "id": "cam1", "kind": "flux-capacitor" });
        let err = validated_body(TypedCollection::Sources, "cam1", &body).unwrap_err();
        assert!(err.to_string().contains("flux-capacitor") || err.to_string().contains("kind"));
    }

    #[test]
    fn rejects_a_non_object_body() {
        let err = validated_body(TypedCollection::Overlays, "o1", &json!("nope")).unwrap_err();
        assert!(err.to_string().contains("JSON object"));
    }

    #[test]
    fn accepts_a_valid_output_with_optional_fields_preserved() {
        let body = json!({
            "id": "web1",
            "kind": "ll_hls",
            "path": "/var/lib/multiview/hls",
            "codec": "h264",
            "part_target_ms": 333
        });
        let stored = validated_body(TypedCollection::Outputs, "web1", &body).unwrap();
        assert_eq!(stored["part_target_ms"], 333);
    }
}
