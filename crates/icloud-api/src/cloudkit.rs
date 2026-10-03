//! `CloudKit` web services (`ckdatabasews`): the record database behind
//! Reminders, Notes and Photos on icloud.com.
//!
//! [`CloudKit`] talks to one container's database, e.g.
//! `com.apple.reminders` / `production` / `private`. It is a port of
//! `pyicloud.common.cloudkit.CloudKitContainerClient` restricted to what the
//! services in this crate use: `records/query`, `records/lookup`,
//! `changes/zone` and `records/modify`.
//!
//! Records are kept close to the wire format: a field is its `CloudKit` type
//! tag plus the raw JSON value, with typed accessors on [`Record`]. Writes
//! are built with [`FieldValue`], which reproduces a pyicloud detail that
//! matters: a field set to "nothing" is sent as `{"type": "TIMESTAMP"}`
//! with no `value` key at all, which is how a due date or a parent
//! reference is cleared.

use std::{collections::BTreeMap, sync::Arc};

use reqwest::header::CONTENT_TYPE;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use crate::{
    error::{Error, Result},
    session::{CT_JSON, Session},
};

/// `0001-01-01T00:00:00Z` in milliseconds, `CloudKit`'s "no date".
const CANONICAL_MIN_MS: i64 = -62_135_596_800_000;
/// Another "no date" value seen in Reminders records, two days earlier.
const OBSERVED_ZERO_MS: i64 = -62_135_769_600_000;

/// Which database of a container to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Database {
    Private,
    Shared,
}

impl Database {
    fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Shared => "shared",
        }
    }
}

/// A record zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneId {
    pub zone_name: String,
    pub zone_type: Option<String>,
    pub owner_record_name: Option<String>,
}

impl ZoneId {
    /// A custom zone of the user's own private database.
    pub fn custom(zone_name: &str) -> Self {
        Self { zone_name: zone_name.into(), zone_type: Some("REGULAR_CUSTOM_ZONE".into()), owner_record_name: None }
    }

    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("zoneName".into(), json!(self.zone_name));
        if let Some(t) = &self.zone_type {
            map.insert("zoneType".into(), json!(t));
        }
        if let Some(o) = &self.owner_record_name {
            map.insert("ownerRecordName".into(), json!(o));
        }
        Value::Object(map)
    }
}

// ---- records -------------------------------------------------------------------

/// One field of a record as `CloudKit` sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// `CloudKit`'s type tag: `STRING`, `INT64`, `TIMESTAMP`, `REFERENCE`, …
    pub kind: String,
    pub value: Value,
}

/// A record returned by a read or a modify.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Record {
    pub record_name: String,
    pub record_type: String,
    pub fields: BTreeMap<String, Field>,
    pub record_change_tag: Option<String>,
    /// When `CloudKit` created the record (its own bookkeeping, not an app field).
    pub created: Option<OffsetDateTime>,
    pub modified: Option<OffsetDateTime>,
}

/// An entry of a `records` array: `CloudKit` mixes records, deletion markers
/// and per-record errors in the same list.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordItem {
    Record(Record),
    Tombstone { record_name: String },
    Error { record_name: Option<String>, code: String, reason: Option<String> },
}

impl RecordItem {
    fn parse(value: &Value) -> Result<Self> {
        let obj = value.as_object().ok_or_else(|| Error::Protocol("CloudKit record entry is not an object".into()))?;
        let text = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_owned);

        if let Some(code) = text("serverErrorCode") {
            return Ok(Self::Error { record_name: text("recordName"), code, reason: text("reason") });
        }
        let record_name =
            text("recordName").ok_or_else(|| Error::Protocol("CloudKit record without recordName".into()))?;
        let Some(record_type) = text("recordType") else {
            if obj.get("deleted").and_then(Value::as_bool) == Some(true) {
                return Ok(Self::Tombstone { record_name });
            }
            return Err(Error::Protocol(format!("CloudKit record {record_name} has no recordType")));
        };

        let fields = obj
            .get("fields")
            .and_then(Value::as_object)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|(name, raw)| {
                        let kind = raw.get("type").and_then(Value::as_str)?.to_owned();
                        let value = raw.get("value").cloned().unwrap_or(Value::Null);
                        Some((name.clone(), Field { kind, value }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let audit = |key: &str| obj.get(key).and_then(|a| a.get("timestamp")).and_then(millis_to_datetime);

        Ok(Self::Record(Record {
            record_name,
            record_type,
            fields,
            record_change_tag: text("recordChangeTag"),
            created: audit("created"),
            modified: audit("modified"),
        }))
    }
}

/// Parse a `records` array and fail on the first per-record error, naming
/// every failed record.
pub(crate) fn records_or_error(items: Vec<RecordItem>, operation: &str) -> Result<Vec<RecordItem>> {
    let errors: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            RecordItem::Error { record_name, code, reason } => Some(format!(
                "{}: {code} ({})",
                record_name.as_deref().unwrap_or("<unknown record>"),
                reason.as_deref().unwrap_or("no reason provided")
            )),
            _ => None,
        })
        .collect();
    if errors.is_empty() {
        Ok(items)
    } else {
        Err(Error::Api {
            code: None,
            reason: format!("{operation} failed for {} record(s): {}", errors.len(), errors.join("; ")),
        })
    }
}

impl Record {
    fn field(&self, key: &str) -> Option<&Value> {
        self.fields.get(key).map(|f| &f.value).filter(|v| !v.is_null())
    }

    /// An `INT64` field (also accepts a numeric string or a boolean).
    pub fn int(&self, key: &str) -> Option<i64> {
        match self.field(key)? {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Value::String(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(i64::from(*b)),
            _ => None,
        }
    }

    pub fn flag(&self, key: &str) -> bool {
        self.int(key).is_some_and(|v| v != 0)
    }

    pub fn double(&self, key: &str) -> Option<f64> {
        match self.field(key)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// A text field. `ENCRYPTED_BYTES` and `BYTES` are base64 on the wire and
    /// are decoded to UTF-8 (invalid sequences replaced).
    pub fn text(&self, key: &str) -> Option<String> {
        let field = self.fields.get(key)?;
        let raw = field.value.as_str()?;
        if matches!(field.kind.as_str(), "ENCRYPTED_BYTES" | "BYTES") {
            let bytes = crate::reminders::crdt::decode_base64(raw)?;
            return Some(String::from_utf8_lossy(&bytes).into_owned());
        }
        Some(raw.to_owned())
    }

    /// The raw string of a field, without any decoding. CRDT documents are
    /// base64 whether typed `STRING` or `ENCRYPTED_BYTES`.
    pub fn raw_str(&self, key: &str) -> Option<&str> {
        self.field(key)?.as_str()
    }

    /// A `TIMESTAMP` field. `CloudKit`'s "no date" sentinels and out-of-range
    /// values read as `None`, as they do in pyicloud.
    pub fn timestamp(&self, key: &str) -> Option<OffsetDateTime> {
        millis_to_datetime(self.field(key)?)
    }

    /// The record name a `REFERENCE` field points to.
    pub fn reference(&self, key: &str) -> Option<String> {
        self.field(key)?.get("recordName").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
    }

    /// A `STRING_LIST` field (empty when absent).
    pub fn string_list(&self, key: &str) -> Vec<String> {
        self.field(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// The download URL of an `ASSET`/`ASSETID` field.
    pub fn asset_url(&self, key: &str) -> Option<String> {
        self.field(key)?.get("downloadURL").and_then(Value::as_str).map(str::to_owned)
    }
}

fn millis_to_datetime(value: &Value) -> Option<OffsetDateTime> {
    let ms = match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?,
        Value::String(s) => {
            let s = s.trim();
            if s.starts_with("0001-01-01") {
                return None;
            }
            s.parse().ok()?
        }
        _ => return None,
    };
    if ms <= CANONICAL_MIN_MS || ms == OBSERVED_ZERO_MS {
        return None;
    }
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

/// Milliseconds since the Unix epoch, the unit of `TIMESTAMP` fields.
pub fn datetime_to_millis(value: OffsetDateTime) -> i64 {
    (value.unix_timestamp_nanos() / 1_000_000) as i64
}

// ---- writes ----------------------------------------------------------------------

/// A field value to write.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Int(i64),
    Str(String),
    StrList(Vec<String>),
    /// Milliseconds since the epoch.
    Timestamp(i64),
    /// Text stored as `ENCRYPTED_BYTES`: base64 of its UTF-8 bytes.
    EncryptedText(String),
    /// A reference with `action: VALIDATE`.
    Reference(String),
    /// Clear a field of the given `CloudKit` type: sent without a `value`.
    Null(&'static str),
}

impl FieldValue {
    fn to_json(&self) -> Value {
        match self {
            Self::Int(v) => json!({"type": "INT64", "value": v}),
            Self::Str(v) => json!({"type": "STRING", "value": v}),
            Self::StrList(v) => json!({"type": "STRING_LIST", "value": v}),
            Self::Timestamp(v) => json!({"type": "TIMESTAMP", "value": v}),
            Self::EncryptedText(v) => {
                use base64::Engine as _;
                json!({"type": "ENCRYPTED_BYTES", "value": base64::engine::general_purpose::STANDARD.encode(v.as_bytes())})
            }
            Self::Reference(name) => json!({"type": "REFERENCE", "value": {"recordName": name, "action": "VALIDATE"}}),
            Self::Null(kind) => json!({"type": kind}),
        }
    }

    pub fn timestamp_or_null(value: Option<OffsetDateTime>) -> Self {
        value.map_or(Self::Null("TIMESTAMP"), |v| Self::Timestamp(datetime_to_millis(v)))
    }

    pub fn str_or_null(value: Option<&str>) -> Self {
        value.filter(|s| !s.is_empty()).map_or(Self::Null("STRING"), |v| Self::Str(v.to_owned()))
    }

    pub fn reference_or_null(value: Option<&str>) -> Self {
        value.filter(|s| !s.is_empty()).map_or(Self::Null("REFERENCE"), |v| Self::Reference(v.to_owned()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    Create,
    Update,
    ForceUpdate,
    Delete,
    ForceDelete,
}

impl OperationType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::ForceUpdate => "forceUpdate",
            Self::Delete => "delete",
            Self::ForceDelete => "forceDelete",
        }
    }
}

/// One record operation of a `records/modify` call. Fields keep their
/// insertion order on the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub kind: OperationType,
    pub record_name: String,
    pub record_type: String,
    pub fields: Vec<(String, FieldValue)>,
    /// The change tag the update is based on; `CloudKit` refuses an update
    /// whose tag is stale.
    pub record_change_tag: Option<String>,
    pub parent: Option<String>,
}

impl Operation {
    fn to_json(&self) -> Value {
        let mut fields = Map::new();
        for (name, value) in &self.fields {
            fields.insert(name.clone(), value.to_json());
        }
        let mut record = Map::new();
        record.insert("recordName".into(), json!(self.record_name));
        record.insert("recordType".into(), json!(self.record_type));
        record.insert("fields".into(), Value::Object(fields));
        record.insert("pluginFields".into(), json!({}));
        if let Some(tag) = &self.record_change_tag {
            record.insert("recordChangeTag".into(), json!(tag));
        }
        if let Some(parent) = &self.parent {
            record.insert("parent".into(), json!({"recordName": parent}));
        }
        json!({"operationType": self.kind.as_str(), "record": record})
    }
}

// ---- requests -------------------------------------------------------------------

/// One page of a `records/query`.
#[derive(Debug, Clone, Default)]
pub struct QueryPage {
    pub records: Vec<RecordItem>,
    pub continuation: Option<String>,
    /// Present because every request asks for `getCurrentSyncToken`.
    pub sync_token: Option<String>,
}

/// What to ask of `changes/zone`.
#[derive(Debug, Clone)]
pub struct ZoneChangesRequest {
    pub zone: ZoneId,
    pub desired_record_types: Option<Vec<String>>,
    pub desired_keys: Option<Vec<String>>,
    pub sync_token: Option<String>,
    pub reverse: Option<bool>,
}

/// One page of `changes/zone`.
#[derive(Debug, Clone, Default)]
pub struct ZoneChangesPage {
    pub records: Vec<RecordItem>,
    pub sync_token: Option<String>,
    pub more_coming: bool,
}

/// One container database, e.g. Reminders' private database.
pub struct CloudKit {
    session: Arc<Session>,
    base: String,
    params: BTreeMap<String, String>,
}

impl std::fmt::Debug for CloudKit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The parameters carry the account's dsid: leave them out.
        f.debug_struct("CloudKit").field("base", &self.base).finish_non_exhaustive()
    }
}

impl CloudKit {
    /// `service_root` is the `ckdatabasews` web service URL from the account
    /// record; `params` the client parameters every iCloud call carries.
    pub fn new(
        session: Arc<Session>,
        service_root: &str,
        container: &str,
        database: Database,
        mut params: BTreeMap<String, String>,
    ) -> Self {
        params.insert("remapEnums".into(), "true".into());
        params.insert("getCurrentSyncToken".into(), "true".into());
        let base =
            format!("{}/database/1/{container}/production/{}", service_root.trim_end_matches('/'), database.as_str());
        Self { session, base, params }
    }

    fn post(&self, operation: &str, body: &Value) -> Result<Value> {
        let request = self
            .session
            .post(&format!("{}/{operation}", self.base))
            .query(&self.params)
            .header(CONTENT_TYPE, CT_JSON)
            .body(serde_json::to_vec(body)?);
        let response = self.session.send_unchecked(request)?;
        // For CloudKit, 401/403 mean the session lapsed or PCS access was
        // withdrawn (pyicloud's CloudKitAuthError). The generic mapping would
        // read the AUTHENTICATION_FAILED body as "service not activated".
        if matches!(response.status, 401 | 403) {
            return Err(Error::AuthRequired(format!("CloudKit rejected the session (HTTP {})", response.status)));
        }
        self.session.check(&response)?;
        response.value()
    }

    fn items(reply: &Value) -> Result<Vec<RecordItem>> {
        reply
            .get("records")
            .and_then(Value::as_array)
            .map_or(Ok(Vec::new()), |items| items.iter().map(RecordItem::parse).collect())
    }

    /// Run a query. `query` is `CloudKit`'s query object, e.g.
    /// `{"recordType": "reminderList", "filterBy": [...]}`.
    pub fn query(
        &self,
        zone: &ZoneId,
        query: Value,
        limit: Option<u32>,
        continuation: Option<&str>,
    ) -> Result<QueryPage> {
        let mut body = Map::new();
        body.insert("query".into(), query);
        body.insert("zoneID".into(), zone.to_json());
        if let Some(limit) = limit {
            body.insert("resultsLimit".into(), json!(limit));
        }
        if let Some(marker) = continuation {
            body.insert("continuationMarker".into(), json!(marker));
        }
        let reply = self.post("records/query", &Value::Object(body))?;
        Ok(QueryPage {
            records: Self::items(&reply)?,
            continuation: reply.get("continuationMarker").and_then(Value::as_str).map(str::to_owned),
            sync_token: reply.get("syncToken").and_then(Value::as_str).map(str::to_owned),
        })
    }

    /// Fetch records by name.
    pub fn lookup(&self, zone: &ZoneId, record_names: &[String]) -> Result<Vec<RecordItem>> {
        let records: Vec<Value> = record_names.iter().map(|name| json!({"recordName": name})).collect();
        let reply = self.post("records/lookup", &json!({"records": records, "zoneID": zone.to_json()}))?;
        Self::items(&reply)
    }

    /// One page of changes in a zone since `request.sync_token`.
    pub fn zone_changes(&self, request: &ZoneChangesRequest) -> Result<ZoneChangesPage> {
        let mut zone = Map::new();
        zone.insert("zoneID".into(), request.zone.to_json());
        if let Some(keys) = &request.desired_keys {
            zone.insert("desiredKeys".into(), json!(keys));
        }
        if let Some(types) = &request.desired_record_types {
            zone.insert("desiredRecordTypes".into(), json!(types));
        }
        if let Some(token) = &request.sync_token {
            zone.insert("syncToken".into(), json!(token));
        }
        if let Some(reverse) = request.reverse {
            zone.insert("reverse".into(), json!(reverse));
        }
        let reply = self.post("changes/zone", &json!({"zones": [zone]}))?;
        let Some(first) = reply.get("zones").and_then(Value::as_array).and_then(|z| z.first()) else {
            return Ok(ZoneChangesPage::default());
        };
        Ok(ZoneChangesPage {
            records: Self::items(first)?,
            sync_token: first.get("syncToken").and_then(Value::as_str).map(str::to_owned),
            more_coming: first.get("moreComing").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// Apply record operations. With `atomic`, `CloudKit` applies all or none.
    pub fn modify(&self, zone: &ZoneId, operations: &[Operation], atomic: Option<bool>) -> Result<Vec<RecordItem>> {
        let mut body = Map::new();
        body.insert("operations".into(), Value::Array(operations.iter().map(Operation::to_json).collect()));
        body.insert("zoneID".into(), zone.to_json());
        if let Some(atomic) = atomic {
            body.insert("atomic".into(), json!(atomic));
        }
        let reply = self.post("records/modify", &Value::Object(body))?;
        Self::items(&reply)
    }

    /// Download an asset (a list's membership file, an attachment).
    pub fn download(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self.session.send(self.session.get(url))?.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: Value) -> Record {
        match RecordItem::parse(&json!({"recordName": "R/1", "recordType": "T", "fields": fields})).unwrap() {
            RecordItem::Record(r) => r,
            other => panic!("not a record: {other:?}"),
        }
    }

    #[test]
    fn entries_are_told_apart() {
        assert!(matches!(
            RecordItem::parse(&json!({"recordName": "A", "deleted": true})).unwrap(),
            RecordItem::Tombstone { record_name } if record_name == "A"
        ));
        assert!(matches!(
            RecordItem::parse(&json!({"recordName": "A", "serverErrorCode": "CONFLICT", "reason": "stale"})).unwrap(),
            RecordItem::Error { code, .. } if code == "CONFLICT"
        ));
        assert!(RecordItem::parse(&json!({"recordName": "A"})).is_err());
    }

    #[test]
    fn no_date_sentinels_read_as_none() {
        let r = record(json!({
            "A": {"type": "TIMESTAMP", "value": -62_135_596_800_000_i64},
            "B": {"type": "TIMESTAMP", "value": -62_135_769_600_000_i64},
            "C": {"type": "TIMESTAMP", "value": "0001-01-01T00:00:00Z"},
            "D": {"type": "TIMESTAMP", "value": 1_790_100_000_000_i64},
            "E": {"type": "TIMESTAMP", "value": i64::MAX},
        }));
        assert_eq!(r.timestamp("A"), None);
        assert_eq!(r.timestamp("B"), None);
        assert_eq!(r.timestamp("C"), None);
        assert_eq!(r.timestamp("D").map(OffsetDateTime::unix_timestamp), Some(1_790_100_000));
        assert_eq!(r.timestamp("E"), None, "out of range is unset, not a failure");
    }

    #[test]
    fn encrypted_bytes_are_base64_text() {
        let r = record(json!({"Name": {"type": "ENCRYPTED_BYTES", "value": "c3Blc2E"}}));
        assert_eq!(r.text("Name").as_deref(), Some("spesa"));
    }

    #[test]
    fn cleared_fields_are_sent_without_a_value() {
        assert_eq!(FieldValue::timestamp_or_null(None).to_json(), json!({"type": "TIMESTAMP"}));
        assert_eq!(FieldValue::reference_or_null(None).to_json(), json!({"type": "REFERENCE"}));
        assert_eq!(
            FieldValue::Reference("List/L1".into()).to_json(),
            json!({"type": "REFERENCE", "value": {"recordName": "List/L1", "action": "VALIDATE"}})
        );
    }

    #[test]
    fn per_record_errors_fail_the_operation() {
        let items = vec![RecordItem::Error { record_name: Some("R/1".into()), code: "CONFLICT".into(), reason: None }];
        let err = records_or_error(items, "Update reminder").unwrap_err().to_string();
        assert!(err.contains("R/1: CONFLICT"), "{err}");
    }
}
