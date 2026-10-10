//! JSON/blobmsg conversion extracted from Cellctl's generic ubus helpers.
//!
//! Enabled by `json`; this module still requires only `alloc`. Integer encoding
//! follows libblobmsg_json: signed 64-bit values, with oversized unsigned values
//! clamped to `i64::MAX`. JSON null is an empty UNSPEC attribute.

use alloc::format;
use serde_json::Value;

use crate::{blobmsg::to_json, BlobBuilder, BlobIter, BlobMsgTable, BlobValue, UbusError};

/// Decode a table's contents, validating every nested attribute first.
pub fn blobmsg_to_json(buf: &[u8]) -> Result<Value, UbusError> {
    validate_blobmsg(buf, 0)?;
    serde_json::from_str(&to_json(buf, true))
        .map_err(|e| UbusError::Method(format!("blobmsg JSON decode failed: {e}")))
}

fn validate_blobmsg(buf: &[u8], depth: usize) -> Result<(), UbusError> {
    if depth > 64 {
        return Err(UbusError::Malformed("blobmsg nesting exceeds 64 levels"));
    }
    let mut it = BlobIter::new(buf);
    while let Some(a) = it.try_next()? {
        match a.value()? {
            BlobValue::Table(p) | BlobValue::Array(p) => validate_blobmsg(p, depth + 1)?,
            BlobValue::Double(value) if !value.is_finite() => {
                return Err(UbusError::Malformed("non-finite double in blobmsg"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Encode a JSON object as an ubus argument or reply table.
pub fn json_to_blobmsg(value: &Value) -> Result<BlobMsgTable, UbusError> {
    if !value.is_object() {
        return Err(UbusError::Method(
            "ubus arguments must be a JSON object".into(),
        ));
    }
    validate_json(value, 0)?;
    Ok(BlobMsgTable::build(|builder| {
        for (name, value) in value.as_object().expect("validated object") {
            push_value(builder, Some(name), value);
        }
    }))
}

fn validate_json(value: &Value, depth: usize) -> Result<(), UbusError> {
    if depth > 64 {
        return Err(UbusError::Method("JSON nesting exceeds 64 levels".into()));
    }
    match value {
        Value::String(s) if s.contains('\0') => {
            return Err(UbusError::Method("ubus strings cannot contain NUL".into()));
        }
        Value::Object(map) => {
            for (key, value) in map {
                if key.contains('\0') || key.len() > u16::MAX as usize {
                    return Err(UbusError::Method("invalid ubus attribute name".into()));
                }
                validate_json(value, depth + 1)?;
            }
        }
        Value::Array(array) => {
            for value in array {
                validate_json(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn push_value(b: &mut BlobBuilder, name: Option<&str>, value: &Value) {
    match value {
        Value::Null => b.put_null(name),
        Value::Bool(x) => b.put_bool(name, *x),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                b.put_i64(name, i);
            } else if let Some(u) = n.as_u64() {
                b.put_i64(name, u.min(i64::MAX as u64) as i64);
            } else if let Some(v) = n.as_f64() {
                b.put_double(name, v);
            }
        }
        Value::String(s) => b.put_str(name, s),
        Value::Array(array) => {
            b.open_array(name);
            for value in array {
                push_value(b, None, value);
            }
            b.close_array();
        }
        Value::Object(map) => {
            b.open_table(name);
            for (name, value) in map {
                push_value(b, Some(name), value);
            }
            b.close_table();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_values_round_trip() {
        let value = json!({"nested": {"array": [null, true, false, -42, 3.5, "héllo"]}});
        let table = json_to_blobmsg(&value).unwrap();
        assert_eq!(blobmsg_to_json(table.contents()).unwrap(), value);
        assert_eq!(blobmsg_to_json(&[]).unwrap(), json!({}));
    }

    #[test]
    fn unsigned_overflow_matches_json_c() {
        let table = json_to_blobmsg(&json!({"n": u64::MAX})).unwrap();
        assert_eq!(
            blobmsg_to_json(table.contents()).unwrap(),
            json!({"n": i64::MAX})
        );
    }

    #[test]
    fn malformed_nested_attributes_are_rejected() {
        let table = json_to_blobmsg(&json!({"nested": {"x": 1}})).unwrap();
        let mut bytes = table.contents().to_vec();
        bytes.pop();
        assert!(blobmsg_to_json(&bytes).is_err());
        let table = BlobMsgTable::build(|b| b.put_double(Some("x"), f64::NAN));
        assert!(blobmsg_to_json(table.contents()).is_err());
    }

    #[test]
    fn rejects_inputs_that_cannot_be_encoded_as_arguments() {
        for value in [
            json!([]),
            json!({"bad\0name": 1}),
            json!({"x": "bad\0value"}),
        ] {
            assert!(json_to_blobmsg(&value).is_err());
        }
    }
}
