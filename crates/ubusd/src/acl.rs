//! ACL registry and loader.
//!
//! This module deliberately holds only the policy model. The broker maps its
//! peer credentials onto [`Cred`] and decides where each protocol operation
//! should be checked.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use thiserror::Error;
use ubus::blobmsg::{BlobBuilder, BlobIter, BlobType, BlobValue};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AclOp {
    Publish,
    Subscribe,
    Listen,
    Send,
    Access,
}

/// A literal exact path or a trailing-star prefix path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pattern {
    pub prefix: String,
    pub partial: bool,
}

impl Pattern {
    pub fn parse(s: &str) -> Self {
        match s.strip_suffix('*') {
            Some(prefix) => Self {
                prefix: prefix.to_string(),
                partial: true,
            },
            None => Self {
                prefix: s.to_string(),
                partial: false,
            },
        }
    }

    pub fn matches(&self, path: &str) -> bool {
        if self.partial {
            path.starts_with(&self.prefix)
        } else {
            path == self.prefix
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessGrant {
    pub object: Pattern,
    /// `None` denotes no methods. Upstream only consults this list when
    /// the ACL entry carried a correctly-typed `methods` array.
    pub methods: Option<Vec<String>>,
    /// Blobmsg TABLE contents for the optional `acl` privilege table.
    pub priv_blob: Option<Vec<u8>>,
}

impl AccessGrant {
    pub fn allows_method(&self, method: &str) -> bool {
        match &self.methods {
            None => false,
            Some(methods) => methods
                .iter()
                .any(|allowed| allowed == "*" || allowed == method),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AclEntry {
    pub user: Option<String>,
    pub group: Option<String>,
    pub publish: Vec<Pattern>,
    pub subscribe: Vec<Pattern>,
    pub listen: Vec<Pattern>,
    pub send: Vec<Pattern>,
    pub access: Vec<AccessGrant>,
}

impl AclEntry {
    pub fn matches_cred(&self, user: Option<&str>, group: Option<&str>) -> bool {
        self.user
            .as_deref()
            .is_some_and(|entry_user| user == Some(entry_user))
            || self
                .group
                .as_deref()
                .is_some_and(|entry_group| group == Some(entry_group))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Cred {
    pub uid: Option<u32>,
    pub user: Option<String>,
    pub group: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AclRegistry {
    pub entries: Vec<AclEntry>,
}

/// One ACL privilege table eligible for `ubus acl query` rendering.
pub struct PrivGrant<'a> {
    pub pattern: &'a Pattern,
    pub user: Option<&'a str>,
    pub group: Option<&'a str>,
    pub blob: &'a [u8],
}

/// Result of scanning an ACL directory. `had_candidates` deliberately means
/// the glob found `*.json` paths, independent of whether their safety checks
/// or JSON parsing later accepted any entries.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AclLoad {
    pub registry: AclRegistry,
    pub had_candidates: bool,
}

/// Apply one upstream-style ACL reload. A glob with no `*.json` candidates
/// returns before `vlist_update`, so it retains the previous registry and
/// does not advance the ACL sequence. Any candidate set replaces it, even
/// when every candidate was rejected and the newly loaded registry is empty.
pub fn apply_reload(current: AclRegistry, load: AclLoad) -> (AclRegistry, bool) {
    if load.had_candidates {
        (load.registry, true)
    } else {
        (current, false)
    }
}

impl AclRegistry {
    pub fn check(&self, cred: &Cred, path: &str, op: AclOp, method: Option<&str>) -> bool {
        if cred.uid == Some(0) {
            return true;
        }

        self.entries.iter().any(|entry| {
            entry.matches_cred(cred.user.as_deref(), cred.group.as_deref())
                && match op {
                    AclOp::Publish => entry.publish.iter().any(|pattern| pattern.matches(path)),
                    AclOp::Subscribe => entry.subscribe.iter().any(|pattern| pattern.matches(path)),
                    AclOp::Listen => entry.listen.iter().any(|pattern| pattern.matches(path)),
                    AclOp::Send => entry.send.iter().any(|pattern| pattern.matches(path)),
                    AclOp::Access => method.is_some_and(|method| {
                        entry
                            .access
                            .iter()
                            .any(|grant| grant.object.matches(path) && grant.allows_method(method))
                    }),
                }
        })
    }

    /// Load every valid root-owned, non-group/other-writable `*.json` file.
    /// Individual files are intentionally best-effort: invalid ACL material
    /// never prevents the broker from starting with the remaining entries.
    pub fn load_dir(dir: &str) -> AclLoad {
        let mut files = match fs::read_dir(dir) {
            Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
            Err(error) => {
                log::warn!("ubusd-rs: unable to read ACL directory {dir}: {error}");
                return AclLoad::default();
            }
        };
        files.retain(|entry| entry.path().extension().is_some_and(|ext| ext == "json"));
        files.sort_by_key(|entry| entry.file_name());

        let had_candidates = !files.is_empty();
        let mut registry = Self::default();
        for entry in files {
            let path = entry.path();
            match load_one(&path) {
                Ok(entries) => registry.entries.extend(entries),
                Err(error) => log::warn!("ubusd-rs: skipping ACL file {}: {error}", path.display()),
            }
        }
        AclLoad {
            registry,
            had_candidates,
        }
    }

    /// All privilege-bearing grants matching `path`, sorted exactly as the
    /// upstream AVL walk: pattern key ascending, preserving load order for
    /// duplicate keys.
    pub fn priv_grants_matching(&self, path: &str) -> Vec<PrivGrant<'_>> {
        let mut grants = self
            .entries
            .iter()
            .flat_map(|entry| {
                entry.access.iter().filter_map(move |grant| {
                    grant
                        .priv_blob
                        .as_deref()
                        .filter(|_| grant.object.matches(path))
                        .map(|blob| PrivGrant {
                            pattern: &grant.object,
                            user: entry.user.as_deref(),
                            group: entry.group.as_deref(),
                            blob,
                        })
                })
            })
            .collect::<Vec<_>>();
        grants.sort_by(|a, b| a.pattern.prefix.cmp(&b.pattern.prefix));
        grants
    }
}

#[derive(Debug, Error)]
pub(crate) enum AclError {
    #[error("path contains an interior NUL")]
    InvalidPath,
    #[error("stat failed: {0}")]
    Stat(#[source] io::Error),
    #[error("ACL file is not a root-owned regular file without group or other write permission")]
    UnsafeFile,
    #[error("read failed: {0}")]
    Read(#[source] io::Error),
    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
}

fn load_one(path: &Path) -> Result<Vec<AclEntry>, AclError> {
    // Use libc::stat rather than std metadata so the stat_spoof test shim
    // interposes this Rust implementation just as it does upstream ubusd.
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| AclError::InvalidPath)?;
    let mut stat = core::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::stat(c_path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return Err(AclError::Stat(io::Error::last_os_error()));
    }
    let stat = unsafe { stat.assume_init() };
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG
        || stat.st_uid != 0
        || stat.st_gid != 0
        || (stat.st_mode & 0o022) != 0
    {
        return Err(AclError::UnsafeFile);
    }

    let bytes = fs::read(path).map_err(AclError::Read)?;
    parse_file(&bytes)
}

#[derive(Deserialize)]
struct RawAclFile {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    publish: Vec<String>,
    #[serde(default)]
    subscribe: Vec<String>,
    #[serde(default)]
    listen: Vec<String>,
    #[serde(default)]
    send: Vec<String>,
    #[serde(default)]
    access: BTreeMap<String, RawAccess>,
}

#[derive(Deserialize)]
struct RawAccess {
    #[serde(default)]
    methods: Option<serde_json::Value>,
    #[serde(default)]
    tags: Option<serde_json::Value>,
    #[serde(default)]
    acl: Option<OrderedJson>,
}

/// JSON value retaining object key order. `serde_json::Value` is unsuitable
/// here because this workspace cannot enable serde_json's `preserve_order`
/// feature without pulling std/indexmap into its no_std build.
#[derive(Clone, Debug)]
enum OrderedJson {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Double(f64),
    String(String),
    Array(Vec<OrderedJson>),
    Object(Vec<(String, OrderedJson)>),
}

impl<'de> Deserialize<'de> for OrderedJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor;
        impl<'de> Visitor<'de> for OrderedVisitor {
            type Value = OrderedJson;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OrderedJson::Null)
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OrderedJson::Null)
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(OrderedJson::Bool(value))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OrderedJson::Int(value))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OrderedJson::UInt(value))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(OrderedJson::Double(value))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(OrderedJson::String(value.to_string()))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(OrderedJson::String(value))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    values.push(value);
                }
                Ok(OrderedJson::Array(values))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some((key, value)) = map.next_entry()? {
                    values.push((key, value));
                }
                Ok(OrderedJson::Object(values))
            }
        }
        deserializer.deserialize_any(OrderedVisitor)
    }
}

fn encode_json_element(
    builder: &mut BlobBuilder,
    name: Option<&str>,
    value: &OrderedJson,
) -> Result<(), AclError> {
    match value {
        OrderedJson::Null => {
            builder.put_null(name);
            Ok(())
        }
        OrderedJson::Bool(value) => {
            builder.put_bool(name, *value);
            Ok(())
        }
        OrderedJson::Int(value) if i32::try_from(*value).is_ok() => {
            builder.put_i32(name, *value as i32);
            Ok(())
        }
        OrderedJson::Int(value) => {
            builder.put_i64(name, *value);
            Ok(())
        }
        OrderedJson::UInt(value) if i32::try_from(*value).is_ok() => {
            builder.put_i32(name, *value as i32);
            Ok(())
        }
        OrderedJson::UInt(value) if i64::try_from(*value).is_ok() => {
            builder.put_i64(name, *value as i64);
            Ok(())
        }
        OrderedJson::UInt(_) => {
            builder.put_i64(name, i64::MAX);
            Ok(())
        }
        OrderedJson::Double(value) => {
            builder.put_double(name, *value);
            Ok(())
        }
        OrderedJson::String(value) => {
            builder.put_str(name, value);
            Ok(())
        }
        OrderedJson::Array(values) => {
            builder.open_array(name);
            for value in values {
                encode_json_element(builder, None, value)?;
            }
            builder.close_array();
            Ok(())
        }
        OrderedJson::Object(values) => {
            builder.open_table(name);
            for (key, value) in values {
                encode_json_element(builder, Some(key), value)?;
            }
            builder.close_table();
            Ok(())
        }
    }
}

fn encode_priv_table(value: OrderedJson) -> Result<Option<Vec<u8>>, AclError> {
    let OrderedJson::Object(values) = value else {
        return Ok(None);
    };
    let mut builder = BlobBuilder::new();
    for (key, value) in &values {
        encode_json_element(&mut builder, Some(key), value)?;
    }
    Ok(Some(builder.finish()))
}

/// Re-encode stored blobmsg contents inside an outer query table. BlobBuilder
/// intentionally has no raw append API, so decoding and rebuilding keeps the
/// `priv_blob` ownership boundary without changing the shared ubus crate.
pub(crate) fn append_priv_blob(builder: &mut BlobBuilder, bytes: &[u8]) -> Result<(), ()> {
    for attr in BlobIter::new(bytes) {
        match attr.ty {
            BlobType::String => match attr.value().map_err(|_| ())? {
                BlobValue::String(value) => builder.put_str(attr.name, value),
                _ => return Err(()),
            },
            BlobType::Int8 => match attr.value().map_err(|_| ())? {
                BlobValue::Int8(value) => builder.put_bool(attr.name, value != 0),
                _ => return Err(()),
            },
            BlobType::Int32 => match attr.value().map_err(|_| ())? {
                BlobValue::Int32(value) => builder.put_i32(attr.name, value),
                _ => return Err(()),
            },
            BlobType::Int64 => match attr.value().map_err(|_| ())? {
                BlobValue::Int64(value) => builder.put_i64(attr.name, value),
                _ => return Err(()),
            },
            BlobType::Double => match attr.value().map_err(|_| ())? {
                BlobValue::Double(value) => builder.put_double(attr.name, value),
                _ => return Err(()),
            },
            BlobType::Table => {
                builder.open_table(attr.name);
                append_priv_blob(builder, attr.payload)?;
                builder.close_table();
            }
            BlobType::Array => {
                builder.open_array(attr.name);
                append_priv_blob(builder, attr.payload)?;
                builder.close_array();
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

pub(crate) fn parse_file(bytes: &[u8]) -> Result<Vec<AclEntry>, AclError> {
    let raw: RawAclFile = serde_json::from_slice(bytes)?;
    let mut access = Vec::with_capacity(raw.access.len());
    for (object, grant) in raw.access {
        let methods = grant.methods.as_ref().and_then(|value| {
            value.as_array().map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
        });
        let has_tags = grant.tags.as_ref().is_some_and(serde_json::Value::is_array);
        let priv_blob = match grant.acl {
            Some(value) => encode_priv_table(value)?,
            None => None,
        };
        if methods.is_some() || has_tags || priv_blob.is_some() {
            access.push(AccessGrant {
                object: Pattern::parse(&object),
                methods,
                priv_blob,
            });
        }
    }
    Ok(vec![AclEntry {
        user: raw.user,
        group: raw.group,
        publish: raw
            .publish
            .iter()
            .map(|pattern| Pattern::parse(pattern))
            .collect(),
        subscribe: raw
            .subscribe
            .iter()
            .map(|pattern| Pattern::parse(pattern))
            .collect(),
        listen: raw
            .listen
            .iter()
            .map(|pattern| Pattern::parse(pattern))
            .collect(),
        send: raw
            .send
            .iter()
            .map(|pattern| Pattern::parse(pattern))
            .collect(),
        access,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(user: Option<&str>, group: Option<&str>) -> AclEntry {
        AclEntry {
            user: user.map(str::to_string),
            group: group.map(str::to_string),
            publish: vec![Pattern::parse("example.*")],
            subscribe: vec![Pattern::parse("pub.sub")],
            listen: vec![Pattern::parse("ev.*")],
            send: vec![Pattern::parse("send.*")],
            access: vec![AccessGrant {
                object: Pattern::parse("svc.*"),
                methods: Some(vec!["ping".to_string()]),
                priv_blob: None,
            }],
        }
    }

    fn cred(user: Option<&str>, group: Option<&str>) -> Cred {
        Cred {
            uid: Some(1000),
            user: user.map(str::to_string),
            group: group.map(str::to_string),
        }
    }

    #[test]
    fn pattern_is_exact_or_trailing_star_prefix_only() {
        assert!(Pattern::parse("exact.one").matches("exact.one"));
        assert!(!Pattern::parse("exact.one").matches("exact.one.child"));
        assert!(Pattern::parse("example.*").matches("example.modem"));
        assert!(!Pattern::parse("exam?le.*").matches("example.modem"));
        assert!(Pattern::parse("cell*ctl.*").matches("cell*ctl.modem"));
    }

    #[test]
    fn publish_subscribe_listen_and_send_check_their_own_grants() {
        let registry = AclRegistry {
            entries: vec![entry(Some("alice"), None)],
        };
        let cred = cred(Some("alice"), None);
        assert!(registry.check(&cred, "example.modem", AclOp::Publish, None));
        assert!(!registry.check(&cred, "other.modem", AclOp::Publish, None));
        assert!(registry.check(&cred, "pub.sub", AclOp::Subscribe, None));
        assert!(!registry.check(&cred, "pub.sub.child", AclOp::Subscribe, None));
        assert!(registry.check(&cred, "ev.changed", AclOp::Listen, None));
        assert!(!registry.check(&cred, "other.changed", AclOp::Listen, None));
        assert!(registry.check(&cred, "send.changed", AclOp::Send, None));
        assert!(!registry.check(&cred, "ev.changed", AclOp::Send, None));
    }

    #[test]
    fn access_requires_matching_object_and_granted_method() {
        let registry = AclRegistry {
            entries: vec![entry(Some("alice"), None)],
        };
        let cred = cred(Some("alice"), None);
        assert!(registry.check(&cred, "svc.one", AclOp::Access, Some("ping")));
        assert!(!registry.check(&cred, "svc.one", AclOp::Access, Some("stop")));
        assert!(!registry.check(&cred, "other.one", AclOp::Access, Some("ping")));
        assert!(!registry.check(&cred, "svc.one", AclOp::Access, None));
    }

    #[test]
    fn access_star_allows_all_but_omitted_methods_denies() {
        let registry = AclRegistry {
            entries: vec![AclEntry {
                access: vec![
                    AccessGrant {
                        object: Pattern::parse("svc.star"),
                        methods: Some(vec!["*".to_string()]),
                        priv_blob: None,
                    },
                    AccessGrant {
                        object: Pattern::parse("svc.omitted"),
                        methods: None,
                        priv_blob: None,
                    },
                ],
                ..entry(Some("alice"), None)
            }],
        };
        let cred = cred(Some("alice"), None);
        assert!(registry.check(&cred, "svc.star", AclOp::Access, Some("any")));
        assert!(!registry.check(&cred, "svc.omitted", AclOp::Access, Some("any")));
    }

    #[test]
    fn credentials_match_user_or_group_but_not_an_unbound_entry() {
        assert!(entry(Some("alice"), None).matches_cred(Some("alice"), None));
        assert!(entry(None, Some("dialout")).matches_cred(None, Some("dialout")));
        let both = entry(Some("alice"), Some("dialout"));
        assert!(both.matches_cred(Some("alice"), Some("other")));
        assert!(both.matches_cred(Some("other"), Some("dialout")));
        assert!(!both.matches_cred(Some("other"), Some("other")));
        assert!(!entry(None, None).matches_cred(Some("alice"), Some("dialout")));
        assert!(!entry(Some("unknown"), None).matches_cred(Some("alice"), None));
    }

    #[test]
    fn root_bypasses_and_an_empty_registry_denies_non_root() {
        let registry = AclRegistry::default();
        assert!(registry.check(
            &Cred {
                uid: Some(0),
                ..Cred::default()
            },
            "anything",
            AclOp::Access,
            None
        ));
        assert!(!registry.check(&cred(Some("alice"), None), "anything", AclOp::Publish, None));
    }

    #[test]
    fn parse_file_obeys_upstream_access_field_types() {
        let entries = parse_file(
            br#"{
                "user": "alice",
                "publish": ["example.*"],
                "access": {
                    "svc.one": { "methods": ["ping"] },
                    "svc.all": { "methods": ["*"] },
                    "svc.string_only": { "methods": "*" },
                    "svc.string_with_tags": { "methods": "*", "tags": [] },
                    "svc.omitted": {}
                },
                "tags": ["ignored"]
            }"#,
        )
        .expect("representative ACL parses");
        let registry = AclRegistry { entries };
        let cred = cred(Some("alice"), None);
        assert!(registry.check(&cred, "example.modem", AclOp::Publish, None));
        assert!(registry.check(&cred, "svc.one", AclOp::Access, Some("ping")));
        assert!(!registry.check(&cred, "svc.one", AclOp::Access, Some("stop")));
        assert!(registry.check(&cred, "svc.all", AclOp::Access, Some("stop")));
        assert!(!registry.check(&cred, "svc.string_only", AclOp::Access, Some("stop")));
        assert!(!registry.check(&cred, "svc.string_with_tags", AclOp::Access, Some("stop")));
        assert!(!registry.check(&cred, "svc.omitted", AclOp::Access, Some("stop")));
        assert_eq!(
            registry.entries[0].access.len(),
            3,
            "empty/string-only entries create no grant"
        );
    }

    #[test]
    fn priv_table_is_encoded_in_file_key_order() {
        use ubus::blobmsg::{lookup, BlobIter, BlobValue};

        let entries =
            parse_file(br#"{"access":{"svc":{"acl":{"zeta":1,"alpha":2}}}}"#).expect("ACL parses");
        let blob = entries[0].access[0]
            .priv_blob
            .as_deref()
            .expect("priv blob");
        let names = BlobIter::new(blob)
            .map(|attr| attr.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec![Some("zeta"), Some("alpha")]);
        assert_eq!(
            lookup(blob, "zeta").and_then(|attr| attr.value().ok()),
            Some(BlobValue::Int32(1))
        );
        assert_eq!(
            &blob[..16],
            &[0x85, 0x00, 0x00, 0x10, 0x00, 0x04, b'z', b'e', b't', b'a', 0, 0, 0, 0, 0, 1,]
        );
    }

    #[test]
    fn priv_table_retains_custom_string_and_int32() {
        use ubus::blobmsg::{lookup, BlobValue};

        let entries = parse_file(br#"{"access":{"svc":{"acl":{"custom":"tag","n":7}}}}"#)
            .expect("ACL parses");
        let blob = entries[0].access[0]
            .priv_blob
            .as_deref()
            .expect("priv blob");
        assert_eq!(
            lookup(blob, "custom").and_then(|attr| attr.value().ok()),
            Some(BlobValue::String("tag"))
        );
        assert_eq!(
            lookup(blob, "n").and_then(|attr| attr.value().ok()),
            Some(BlobValue::Int32(7))
        );
    }

    #[test]
    fn encode_priv_table_byte_parity_with_reference() {
        if !ubus_testkit::json::AVAILABLE_JSON {
            ubus_testkit::skip::or_skip(
                "acl encode_priv_table_byte_parity_with_reference",
                "libblobmsg_json unavailable",
            );
            return;
        }

        let cases = [
            r#"{"s":"he said \"hi\" \\ a/b é"}"#,
            r#"{"": ""}"#,
            r#"{"n":2147483647}"#,
            r#"{"n":2147483648}"#,
            r#"{"n":-2147483648}"#,
            r#"{"n":-2147483649}"#,
            r#"{"n":-1}"#,
            r#"{"n":0}"#,
            r#"{"n":9223372036854775807}"#,
            r#"{"n":null}"#,
            r#"{"u":18446744073709551615}"#,
            r#"{"t":true,"f":false}"#,
            r#"{"n":0.1}"#,
            r#"{"n":1.5}"#,
            r#"{"n":1e20}"#,
            r#"{"t":{"x":[1,2,"three"]}}"#,
            r#"{"object":{},"array":[]}"#,
            r#"{}"#,
        ];

        for case in cases {
            let value: OrderedJson = serde_json::from_str(case).expect("parse ordered JSON");
            let mine = encode_priv_table(value)
                .expect("encode privilege table")
                .expect("object produces privilege table");
            let reference = ubus_testkit::json::reference_from_json(case).expect("reference JSON");
            assert_eq!(mine, reference.inner_bytes, "case {case}");
        }
    }

    #[test]
    fn encode_priv_table_null_and_u64_overflow_match_json_c_without_reference() {
        let null: OrderedJson = serde_json::from_str(r#"{"n":null}"#).expect("parse null");
        assert_eq!(
            encode_priv_table(null).unwrap().unwrap(),
            vec![0x80, 0, 0, 0x08, 0, 1, b'n', 0]
        );

        let overflow: OrderedJson =
            serde_json::from_str(r#"{"u":18446744073709551615}"#).expect("parse u64");
        assert_eq!(
            encode_priv_table(overflow).unwrap().unwrap(),
            vec![0x84, 0, 0, 0x10, 0, 1, b'u', 0, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,]
        );
    }

    #[test]
    fn load_dir_of_a_missing_directory_is_an_empty_registry() {
        let path =
            std::env::temp_dir().join(format!("ubusd-rs-acl-missing-{}", std::process::id()));
        let load = AclRegistry::load_dir(path.to_str().expect("temp path is utf-8"));
        assert!(load.registry.entries.is_empty());
        assert!(!load.had_candidates);
    }

    #[test]
    fn apply_reload_keeps_current_without_candidates_and_replaces_with_candidates() {
        let current = AclRegistry {
            entries: vec![entry(Some("current"), None)],
        };
        let (kept, bump) = apply_reload(
            current.clone(),
            AclLoad {
                registry: AclRegistry::default(),
                had_candidates: false,
            },
        );
        assert_eq!(kept, current);
        assert!(!bump);

        let (replaced, bump) = apply_reload(
            current,
            AclLoad {
                registry: AclRegistry::default(),
                had_candidates: true,
            },
        );
        assert!(
            replaced.entries.is_empty(),
            "candidates replace even when rejected"
        );
        assert!(bump);
    }
}
