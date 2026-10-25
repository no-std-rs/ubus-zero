//! Differential parity for ubus-zero's blob-to-JSON renderer against the real
//! `libblobmsg_json` implementation.
#![cfg(target_os = "linux")]

use std::fmt::Write;

use ubus::blobmsg::{BlobBuilder, BlobMsgTable};
use ubus_testkit::json;

const SEEDS: u64 = 512;

// ---- deterministic PRNG (SplitMix64) --------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Fold the seed so seed 0 doesn't degenerate to an all-zero
        // stream; SplitMix64 is otherwise fine from any state.
        Self(seed ^ 0xA0761D6478BD642F)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (n > 0).
    fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % u64::from(n)) as u32
    }
}

#[derive(Debug)]
enum JsonValue {
    String(String),
    Int(i64),
    Double(f64),
    Bool(bool),
    Null,
    Object(Vec<(String, Self)>),
    Array(Vec<Self>),
}

#[derive(Debug)]
enum BuilderValue {
    String(String),
    Int(i64),
    Double(f64),
    Bool(bool),
    Object(Vec<(String, Self)>),
    Array(Vec<Self>),
}

const SAFE_CHARS: &[char] = &[
    ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', '0', '1', '2',
    '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', '@', 'A', 'B', 'C', 'D', 'E',
    'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X',
    'Y', 'Z', '[', '\\', ']', '^', '_', '`', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k',
    'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', '{', '|', '}', '~',
    'é', '☕', '\u{0008}', '\t', '\n', '\r', '\u{0001}', '\u{001f}', '\u{007f}',
];

const DOUBLE_BOUNDARIES: [f64; 22] = [
    0.1,
    1e20,
    100.0,
    f64::from_bits(1),
    core::f64::consts::PI,
    0.30000000000000004,
    1e308,
    -2.75,
    0.001,
    1e-5,
    1.25e-4,
    0.0001,
    1e17,
    123456789012345680.0,
    f64::from_bits(0x000f_ffff_ffff_ffff),
    0.0,
    -0.0,
    1.5,
    f64::NAN,
    f64::from_bits(0xfff8_0000_0000_0000),
    f64::INFINITY,
    f64::NEG_INFINITY,
];

fn safe_string(rng: &mut Rng) -> String {
    let mut s = String::new();
    for _ in 0..rng.below(9) {
        s.push(SAFE_CHARS[rng.below(SAFE_CHARS.len() as u32) as usize]);
    }
    s
}

fn unique_key(rng: &mut Rng, used: &[String]) -> String {
    let mut key = safe_string(rng);
    if !used.contains(&key) {
        return key;
    }

    let base = key.clone();
    let mut suffix = 1;
    loop {
        key = format!("{base}{suffix}");
        if !used.contains(&key) {
            return key;
        }
        suffix += 1;
    }
}

fn biased_int(rng: &mut Rng) -> i64 {
    const BOUNDARIES: [i64; 10] = [
        0,
        1,
        -1,
        i32::MAX as i64,
        i32::MAX as i64 + 1,
        i32::MIN as i64,
        i32::MIN as i64 - 1,
        i64::MAX,
        i64::MIN,
        42,
    ];

    if rng.below(3) != 0 {
        BOUNDARIES[rng.below(BOUNDARIES.len() as u32) as usize]
    } else {
        rng.next_u64() as i64
    }
}

fn finite_double(rng: &mut Rng) -> f64 {
    if rng.below(3) != 0 {
        loop {
            let value = DOUBLE_BOUNDARIES[rng.below(DOUBLE_BOUNDARIES.len() as u32) as usize];
            if value.is_finite() {
                return value;
            }
        }
    }

    loop {
        let value = f64::from_bits(rng.next_u64());
        if value.is_finite() {
            return value;
        }
    }
}

fn full_double(rng: &mut Rng) -> f64 {
    if rng.below(3) != 0 {
        DOUBLE_BOUNDARIES[rng.below(DOUBLE_BOUNDARIES.len() as u32) as usize]
    } else {
        f64::from_bits(rng.next_u64())
    }
}

fn json_object(rng: &mut Rng, depth: u8) -> Vec<(String, JsonValue)> {
    let mut members: Vec<(String, JsonValue)> = Vec::new();
    for _ in 0..rng.below(7) {
        let used: Vec<String> = members.iter().map(|(key, _)| key.clone()).collect();
        let key = unique_key(rng, &used);
        members.push((key, json_value(rng, depth)));
    }
    members
}

fn json_value(rng: &mut Rng, depth: u8) -> JsonValue {
    match if depth == 3 {
        rng.below(5)
    } else {
        rng.below(7)
    } {
        0 => JsonValue::String(safe_string(rng)),
        1 => JsonValue::Int(biased_int(rng)),
        2 => JsonValue::Double(finite_double(rng)),
        3 => JsonValue::Bool(rng.below(2) != 0),
        4 => JsonValue::Null,
        5 => JsonValue::Object(json_object(rng, depth + 1)),
        _ => JsonValue::Array(
            (0..rng.below(5))
                .map(|_| json_value(rng, depth + 1))
                .collect(),
        ),
    }
}

fn builder_object(rng: &mut Rng, depth: u8) -> Vec<(String, BuilderValue)> {
    let mut members: Vec<(String, BuilderValue)> = Vec::new();
    for _ in 0..rng.below(7) {
        let used: Vec<String> = members.iter().map(|(key, _)| key.clone()).collect();
        let key = unique_key(rng, &used);
        members.push((key, builder_value(rng, depth)));
    }
    members
}

fn builder_value(rng: &mut Rng, depth: u8) -> BuilderValue {
    match if depth == 3 {
        rng.below(4)
    } else {
        rng.below(6)
    } {
        0 => BuilderValue::String(safe_string(rng)),
        1 => BuilderValue::Int(biased_int(rng)),
        2 => BuilderValue::Double(full_double(rng)),
        3 => BuilderValue::Bool(rng.below(2) != 0),
        4 => BuilderValue::Object(builder_object(rng, depth + 1)),
        _ => BuilderValue::Array(
            (0..rng.below(5))
                .map(|_| builder_value(rng, depth + 1))
                .collect(),
        ),
    }
}

fn json_escape_into(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{:04x}", c as u32).expect("write to String");
            }
            c => out.push(c),
        }
    }
}

fn render_json(value: &JsonValue, out: &mut String) {
    match value {
        JsonValue::String(value) => {
            out.push('"');
            json_escape_into(out, value);
            out.push('"');
        }
        JsonValue::Int(value) => write!(out, "{value}").expect("write to String"),
        JsonValue::Double(value) => write!(out, "{value:?}").expect("write to String"),
        JsonValue::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        JsonValue::Null => out.push_str("null"),
        JsonValue::Object(members) => render_json_object_into(members, out),
        JsonValue::Array(values) => {
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                render_json(value, out);
            }
            out.push(']');
        }
    }
}

fn render_json_object_into(members: &[(String, JsonValue)], out: &mut String) {
    out.push('{');
    for (index, (key, value)) in members.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push('"');
        json_escape_into(out, key);
        out.push_str("\":");
        render_json(value, out);
    }
    out.push('}');
}

fn render_json_object(members: &[(String, JsonValue)]) -> String {
    let mut rendered = String::new();
    render_json_object_into(members, &mut rendered);
    rendered
}

fn put_builder_value(builder: &mut BlobBuilder, name: Option<&str>, value: &BuilderValue) {
    match value {
        BuilderValue::String(value) => builder.put_str(name, value),
        BuilderValue::Int(value) if i32::try_from(*value).is_ok() => {
            builder.put_i32(name, *value as i32);
        }
        BuilderValue::Int(value) => builder.put_i64(name, *value),
        BuilderValue::Double(value) => builder.put_double(name, *value),
        BuilderValue::Bool(value) => builder.put_bool(name, *value),
        BuilderValue::Object(members) => {
            builder.open_table(name);
            for (key, value) in members {
                put_builder_value(builder, Some(key), value);
            }
            builder.close_table();
        }
        BuilderValue::Array(values) => {
            builder.open_array(name);
            for value in values {
                put_builder_value(builder, None, value);
            }
            builder.close_array();
        }
    }
}

fn raw_blobmsg_attr(ty: u8, name: Option<&str>, payload: &[u8]) -> Vec<u8> {
    let name = name.unwrap_or("").as_bytes();
    let name_len: u16 = name.len().try_into().expect("short test attr name");
    let name_header_len = 2 + name.len() + 1;
    let name_padding = (4 - (name_header_len & 3)) & 3;
    let total = 4 + name_header_len + name_padding + payload.len();
    let id_len = 0x8000_0000 | (u32::from(ty) << 24) | total as u32;
    let mut attr = Vec::with_capacity(total + 3);
    attr.extend_from_slice(&id_len.to_be_bytes());
    attr.extend_from_slice(&name_len.to_be_bytes());
    attr.extend_from_slice(name);
    attr.push(0);
    attr.resize(attr.len() + name_padding, 0);
    attr.extend_from_slice(payload);
    attr.resize(attr.len() + ((4 - (total & 3)) & 3), 0);
    attr
}

#[test]
fn blob_to_json_parity_from_json_inputs() {
    if !ubus_testkit::json::AVAILABLE_JSON {
        ubus_testkit::skip::or_skip(
            "blob_to_json_parity_from_json_inputs",
            "libblobmsg_json reference unavailable (.dep)",
        );
        return;
    }

    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let tree = json_object(&mut rng, 0);
        let rendered = render_json_object(&tree);
        let reference =
            json::reference_from_json(&rendered).expect("reference should accept generated object");
        assert_eq!(
            ubus::blobmsg::to_json(&reference.inner_bytes, true),
            reference.formatted,
            "seed {seed}, input {rendered}"
        );
    }
}

#[test]
fn blob_to_json_parity_from_builder_trees() {
    if !ubus_testkit::json::AVAILABLE_JSON {
        ubus_testkit::skip::or_skip(
            "blob_to_json_parity_from_builder_trees",
            "libblobmsg_json reference unavailable (.dep)",
        );
        return;
    }

    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let tree = builder_object(&mut rng, 0);
        let table = BlobMsgTable::build(|builder| {
            for (key, value) in &tree {
                put_builder_value(builder, Some(key), value);
            }
        });
        let reference = json::format_attr_bytes(table.as_slice(), true)
            .expect("reference should format builder table");
        assert_eq!(
            ubus::blobmsg::to_json(table.contents(), true),
            reference,
            "seed {seed}"
        );
    }
}

#[test]
fn promoted_divergence_regressions() {
    if !ubus_testkit::json::AVAILABLE_JSON {
        ubus_testkit::skip::or_skip(
            "promoted_divergence_regressions",
            "libblobmsg_json reference unavailable (.dep)",
        );
        return;
    }

    for value in DOUBLE_BOUNDARIES {
        let table = BlobMsgTable::build(|builder| builder.put_double(Some("value"), value));
        let actual = ubus::blobmsg::to_json(table.contents(), true);
        let reference = json::format_attr_bytes(table.as_slice(), true)
            .expect("reference should format double table");
        assert_eq!(actual, reference, "double {value:?}");
    }

    let table = BlobMsgTable::build(|builder| builder.put_str(Some("value"), "\u{0008}"));
    let actual = ubus::blobmsg::to_json(table.contents(), true);
    let reference = json::format_attr_bytes(table.as_slice(), true)
        .expect("reference should format backspace table");
    assert_eq!(actual, reference, "string backspace");

    let int8 = raw_blobmsg_attr(7, Some("value"), &[5]);
    let table = raw_blobmsg_attr(2, None, &int8);
    let actual = ubus::blobmsg::to_json(&int8, true);
    let reference =
        json::format_attr_bytes(&table, true).expect("reference should format raw int8 table");
    assert_eq!(actual, reference, "int8 5");
}
