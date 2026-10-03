//! iCloud Reminders over `CloudKit` (`com.apple.reminders`, zone `Reminders`).
//!
//! A port of `pyicloud.services.reminders` (2.7.0) restricted to what a
//! two-way sync with a `CalDAV` task list needs:
//!
//! * reads: [`RemindersService::lists`], [`RemindersService::list_contents`]
//!   (reminders of a list together with their hashtags, recurrence rules,
//!   alarms and URL attachments), [`RemindersService::get`];
//! * incremental sync: [`RemindersService::sync_cursor`] and
//!   [`RemindersService::changes_since`];
//! * writes: create, update and (soft) delete reminders; create and delete
//!   hashtags; create, update and delete recurrence rules.
//!
//! The requests are the ones pyicloud sends, checked field for field against
//! a capture of pyicloud 2.7.0 (`tests/reminders_flow.rs`). Two behaviours
//! are copied on purpose, not by accident:
//!
//! * deletion is a soft delete (`Deleted = 1`), as on icloud.com;
//! * every write carries a `ResolutionTokenMap` naming the fields it changed,
//!   each with `counter: 1`, a fresh replica id and the current time. Apple's
//!   devices use these tokens to merge concurrent edits field by field.
//!   Whether `counter: 1` always wins against an edit made on an iPhone is
//!   not established; it is what pyicloud does.

pub mod crdt;

use std::{collections::BTreeMap, fmt, fmt::Write as _, sync::Arc};

use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::{
    cloudkit::{
        CloudKit, FieldValue, Operation, OperationType, Record, RecordItem, ZoneChangesRequest, ZoneId,
        datetime_to_millis, records_or_error,
    },
    error::{Error, Result},
};

/// `CloudKit` container of Reminders.
pub const CONTAINER: &str = "com.apple.reminders";
/// Name of the PCS application to ask for before touching the container.
pub const PCS_APP: &str = "reminders";
const ZONE: &str = "Reminders";
/// Seconds between the Unix epoch and Apple's (2001-01-01).
const APPLE_EPOCH_OFFSET: f64 = 978_307_200.0;
const PAGE: u32 = 200;

// ---- model -----------------------------------------------------------------------

/// A reminders list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemindersList {
    /// Record name, e.g. `List/6F3C…`.
    pub id: String,
    pub title: String,
    /// Apple's colour description: a JSON object as text.
    pub color: Option<String>,
    pub count: usize,
    pub badge_emblem: Option<String>,
    pub sorting_style: Option<String>,
    pub is_group: bool,
    pub deleted: bool,
    /// Raw reminder ids (without the `Reminder/` prefix), in Apple's order.
    pub reminder_ids: Vec<String>,
    pub record_change_tag: Option<String>,
}

/// A reminder.
#[derive(Debug, Clone, PartialEq, Default)]
#[allow(clippy::struct_excessive_bools, reason = "mirrors the record's flags one to one")]
pub struct Reminder {
    /// Record name, e.g. `Reminder/1D2E…`.
    pub id: String,
    /// Record name of the list.
    pub list_id: String,
    pub title: String,
    pub notes: String,
    pub completed: bool,
    pub completed_date: Option<OffsetDateTime>,
    pub due_date: Option<OffsetDateTime>,
    pub start_date: Option<OffsetDateTime>,
    /// 0 none, 1 high, 5 medium, 9 low (iCalendar's scale).
    pub priority: i64,
    pub flagged: bool,
    pub all_day: bool,
    pub deleted: bool,
    pub time_zone: Option<String>,
    pub alarm_ids: Vec<String>,
    pub hashtag_ids: Vec<String>,
    pub attachment_ids: Vec<String>,
    pub recurrence_rule_ids: Vec<String>,
    /// Record name of the parent reminder, for subtasks.
    pub parent_reminder_id: Option<String>,
    pub created: Option<OffsetDateTime>,
    pub modified: Option<OffsetDateTime>,
    pub record_change_tag: Option<String>,
}

/// One entry of [`RemindersService::changes_since`].
#[derive(Debug, Clone, PartialEq)]
pub enum ReminderChange {
    Updated(Box<Reminder>),
    /// Soft-deleted or purged; carries the record name.
    Deleted {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hashtag {
    pub id: String,
    pub name: String,
    pub reminder_id: String,
    pub record_change_tag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Frequency {
    #[default]
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Frequency {
    fn from_raw(raw: i64) -> Self {
        match raw {
            2 => Self::Weekly,
            3 => Self::Monthly,
            4 => Self::Yearly,
            _ => Self::Daily,
        }
    }

    fn raw(self) -> i64 {
        match self {
            Self::Daily => 1,
            Self::Weekly => 2,
            Self::Monthly => 3,
            Self::Yearly => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecurrenceRule {
    pub id: String,
    pub reminder_id: String,
    pub frequency: Frequency,
    pub interval: u32,
    /// 0 means "repeat forever".
    pub occurrence_count: u32,
    /// 0 = Sunday … 6 = Saturday.
    pub first_day_of_week: u8,
    pub record_change_tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Alarm {
    pub id: String,
    pub alarm_uid: String,
    pub reminder_id: String,
    pub trigger_id: String,
    pub record_change_tag: Option<String>,
}

/// A geofence alarm. The only trigger kind Reminders stores as a record:
/// time alerts derive from the due date.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LocationTrigger {
    pub id: String,
    pub alarm_id: String,
    pub title: String,
    pub address: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius: f64,
    /// 1 arriving, 2 leaving.
    pub proximity: i64,
    pub location_uid: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UrlAttachment {
    pub id: String,
    pub reminder_id: String,
    pub url: String,
}

/// Everything a list holds, as returned by one compound query.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListContents {
    pub reminders: Vec<Reminder>,
    pub hashtags: BTreeMap<String, Hashtag>,
    pub recurrence_rules: BTreeMap<String, RecurrenceRule>,
    pub alarms: BTreeMap<String, Alarm>,
    pub triggers: BTreeMap<String, LocationTrigger>,
    pub url_attachments: BTreeMap<String, UrlAttachment>,
}

/// Fields of a new reminder.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewReminder {
    pub list_id: String,
    pub title: String,
    pub notes: String,
    pub completed: bool,
    pub due_date: Option<OffsetDateTime>,
    pub priority: i64,
    pub flagged: bool,
    pub all_day: bool,
    pub time_zone: Option<String>,
    pub parent_reminder_id: Option<String>,
}

// ---- ids -------------------------------------------------------------------------

/// `Prefix/value` from either form.
pub fn record_name(value: &str, prefix: &str) -> String {
    if value.is_empty() || value.starts_with(&format!("{prefix}/")) {
        value.to_owned()
    } else {
        format!("{prefix}/{value}")
    }
}

/// `value` without its `Prefix/`.
pub fn raw_id(value: &str, prefix: &str) -> String {
    value.strip_prefix(&format!("{prefix}/")).unwrap_or(value).to_owned()
}

// ---- record mapping --------------------------------------------------------------

fn record_to_reminder(rec: &Record) -> Reminder {
    let crdt_text = |key: &str, fallback: &str| match rec.raw_str(key) {
        None | Some("") => fallback.to_owned(),
        Some(wire) => crdt::decode_wire(wire).unwrap_or_else(|err| {
            tracing::warn!("{key} of {} could not be decoded: {err}", rec.record_name);
            if key == "TitleDocument" { "Error Decoding Title".to_owned() } else { String::new() }
        }),
    };
    let ids = |key: &str, prefix: &str| rec.string_list(key).iter().map(|id| raw_id(id, prefix)).collect();

    Reminder {
        id: rec.record_name.clone(),
        list_id: rec.reference("List").unwrap_or_default(),
        title: crdt_text("TitleDocument", "Untitled"),
        notes: crdt_text("NotesDocument", ""),
        completed: rec.flag("Completed"),
        completed_date: rec.timestamp("CompletionDate"),
        due_date: rec.timestamp("DueDate"),
        start_date: rec.timestamp("StartDate"),
        priority: rec.int("Priority").unwrap_or(0),
        flagged: rec.flag("Flagged"),
        all_day: rec.flag("AllDay"),
        deleted: rec.flag("Deleted"),
        time_zone: rec.text("TimeZone").filter(|s| !s.is_empty()),
        alarm_ids: ids("AlarmIDs", "Alarm"),
        hashtag_ids: ids("HashtagIDs", "Hashtag"),
        attachment_ids: ids("AttachmentIDs", "Attachment"),
        recurrence_rule_ids: ids("RecurrenceRuleIDs", "RecurrenceRule"),
        parent_reminder_id: rec.reference("ParentReminder"),
        created: rec.timestamp("CreationDate").or(rec.created),
        modified: rec.timestamp("LastModifiedDate").or(rec.modified),
        record_change_tag: rec.record_change_tag.clone(),
    }
}

fn record_to_hashtag(rec: &Record) -> Hashtag {
    Hashtag {
        id: rec.record_name.clone(),
        name: rec.text("Name").unwrap_or_default(),
        reminder_id: rec.reference("Reminder").unwrap_or_default(),
        record_change_tag: rec.record_change_tag.clone(),
    }
}

fn record_to_recurrence_rule(rec: &Record) -> RecurrenceRule {
    let small = |key: &str, default: i64| u32::try_from(rec.int(key).unwrap_or(default)).unwrap_or(0);
    RecurrenceRule {
        id: rec.record_name.clone(),
        reminder_id: rec.reference("Reminder").unwrap_or_default(),
        frequency: Frequency::from_raw(rec.int("Frequency").unwrap_or(1)),
        interval: small("Interval", 1).max(1),
        occurrence_count: small("OccurrenceCount", 0),
        first_day_of_week: u8::try_from(small("FirstDayOfTheWeek", 0).min(6)).unwrap_or(0),
        record_change_tag: rec.record_change_tag.clone(),
    }
}

fn record_to_alarm(rec: &Record) -> Alarm {
    Alarm {
        id: rec.record_name.clone(),
        alarm_uid: rec.text("AlarmUID").unwrap_or_default(),
        reminder_id: rec.reference("Reminder").unwrap_or_default(),
        trigger_id: rec.text("TriggerID").unwrap_or_default(),
        record_change_tag: rec.record_change_tag.clone(),
    }
}

fn record_to_trigger(rec: &Record) -> Option<LocationTrigger> {
    if rec.text("Type").as_deref() != Some("Location") {
        return None;
    }
    Some(LocationTrigger {
        id: rec.record_name.clone(),
        alarm_id: rec.reference("Alarm").unwrap_or_default(),
        title: rec.text("Title").unwrap_or_default(),
        address: rec.text("Address").unwrap_or_default(),
        latitude: rec.double("Latitude").unwrap_or(0.0),
        longitude: rec.double("Longitude").unwrap_or(0.0),
        radius: rec.double("Radius").unwrap_or(0.0),
        proximity: rec.int("Proximity").filter(|p| matches!(p, 1 | 2)).unwrap_or(1),
        location_uid: rec.text("LocationUID").unwrap_or_default(),
    })
}

fn record_to_url_attachment(rec: &Record) -> Option<UrlAttachment> {
    if rec.text("Type").as_deref() != Some("URL") {
        return None;
    }
    let raw = rec.text("URL").unwrap_or_default();
    Some(UrlAttachment {
        id: rec.record_name.clone(),
        reminder_id: rec.reference("Reminder").unwrap_or_default(),
        url: decode_attachment_url(&raw),
    })
}

/// URL attachments are stored either as the URL or as base64 of it.
fn decode_attachment_url(value: &str) -> String {
    if value.is_empty() || looks_like_url(value) {
        return value.to_owned();
    }
    crdt::decode_base64(value)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|decoded| looks_like_url(decoded))
        .unwrap_or_else(|| value.to_owned())
}

fn looks_like_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| match u.scheme() {
        "http" | "https" => u.host_str().is_some_and(|h| !h.is_empty()),
        _ => u.has_host() || !u.path().is_empty(),
    })
}

fn parse_reminder_ids(text: &str, source: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(text).map_err(|_| Error::Protocol(format!("{source} is not JSON")))?;
    let items = value.as_array().ok_or_else(|| Error::Protocol(format!("{source} is not a JSON array")))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(|id| raw_id(id, "Reminder"))
                .ok_or_else(|| Error::Protocol(format!("{source} holds a non-string reminder id")))
        })
        .collect()
}

// ---- service -----------------------------------------------------------------------

type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;
type IdSource = Arc<dyn Fn() -> Result<String> + Send + Sync>;

/// The Reminders service. Obtain it from [`crate::Client::reminders`].
pub struct RemindersService {
    ck: CloudKit,
    zone: ZoneId,
    now: Clock,
    new_id: IdSource,
}

impl fmt::Debug for RemindersService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemindersService").field("ck", &self.ck).finish_non_exhaustive()
    }
}

impl RemindersService {
    pub fn new(ck: CloudKit) -> Self {
        Self::with_clock_and_ids(ck, OffsetDateTime::now_utc, || crate::uuid_v4().map(|id| id.to_uppercase()))
    }

    /// Like [`new`](Self::new) with a fixed clock and id source, so tests can
    /// compare requests with recorded ones.
    #[doc(hidden)]
    pub fn with_clock_and_ids(
        ck: CloudKit,
        now: impl Fn() -> OffsetDateTime + Send + Sync + 'static,
        new_id: impl Fn() -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self { ck, zone: ZoneId::custom(ZONE), now: Arc::new(now), new_id: Arc::new(new_id) }
    }

    // ---- reads ---------------------------------------------------------------

    /// Every page of `changes/zone` for the given record types.
    fn zone_changes(
        &self,
        types: Option<Vec<String>>,
        keys: Option<Vec<String>>,
        since: Option<String>,
        reverse: Option<bool>,
    ) -> Result<(Vec<RecordItem>, Option<String>)> {
        let mut request = ZoneChangesRequest {
            zone: self.zone.clone(),
            desired_record_types: types,
            desired_keys: keys,
            sync_token: since,
            reverse,
        };
        let mut records = Vec::new();
        loop {
            let page = self.ck.zone_changes(&request)?;
            records.extend(page.records);
            if page.sync_token.is_some() {
                request.sync_token = page.sync_token;
            }
            if !page.more_coming {
                return Ok((records, request.sync_token));
            }
        }
    }

    /// All lists, deleted ones included (see [`RemindersList::deleted`]).
    pub fn lists(&self) -> Result<Vec<RemindersList>> {
        let (items, _) = self.zone_changes(Some(vec!["List".into()]), None, None, None)?;
        records_or_error(items, "Fetch reminder lists")?
            .into_iter()
            .filter_map(|item| match item {
                RecordItem::Record(rec) if rec.record_type == "List" => Some(rec),
                _ => None,
            })
            .map(|rec| self.record_to_list(&rec))
            .collect()
    }

    fn record_to_list(&self, rec: &Record) -> Result<RemindersList> {
        let reminder_ids = if let Some(text) = rec.raw_str("ReminderIDs") {
            parse_reminder_ids(text, &format!("List {} ReminderIDs", rec.record_name))?
        } else if let Some(url) = rec.asset_url("ReminderIDsAsset") {
            let bytes = self.ck.download(&url)?;
            let text = String::from_utf8(bytes)
                .map_err(|_| Error::Protocol(format!("List {} ReminderIDsAsset is not UTF-8", rec.record_name)))?;
            parse_reminder_ids(&text, &format!("List {} ReminderIDsAsset", rec.record_name))?
        } else {
            Vec::new()
        };
        let mut count = usize::try_from(rec.int("Count").unwrap_or(0)).unwrap_or(0);
        if count == 0 {
            // Live list records can carry the full membership while Count stays 0.
            count = reminder_ids.len();
        }
        Ok(RemindersList {
            id: rec.record_name.clone(),
            title: rec.text("Name").filter(|s| !s.is_empty()).unwrap_or_else(|| "Untitled".into()),
            color: rec.text("Color").filter(|s| !s.is_empty()),
            count,
            badge_emblem: rec.text("BadgeEmblem"),
            sorting_style: rec.text("SortingStyle"),
            is_group: rec.flag("IsGroup"),
            deleted: rec.flag("Deleted"),
            reminder_ids,
            record_change_tag: rec.record_change_tag.clone(),
        })
    }

    /// The reminders of a list (completed ones included) with their linked
    /// records, from Apple's compound `reminderList` query.
    pub fn list_contents(&self, list_id: &str) -> Result<ListContents> {
        let query = json!({
            "recordType": "reminderList",
            "filterBy": [
                {"comparator": "EQUALS", "fieldName": "List",
                 "fieldValue": {"type": "REFERENCE", "value": {"recordName": list_id, "action": "VALIDATE"}}},
                {"comparator": "EQUALS", "fieldName": "includeCompleted", "fieldValue": {"type": "INT64", "value": 1}},
                {"comparator": "EQUALS", "fieldName": "LookupValidatingReference", "fieldValue": {"type": "INT64", "value": 1}},
            ],
        });

        let mut reminders: BTreeMap<String, Reminder> = BTreeMap::new();
        let mut out = ListContents::default();
        let mut continuation: Option<String> = None;
        loop {
            let page = self.ck.query(&self.zone, query.clone(), Some(PAGE), continuation.as_deref())?;
            for item in records_or_error(page.records, "List reminders query")? {
                let RecordItem::Record(rec) = item else { continue };
                match rec.record_type.as_str() {
                    "Reminder" => {
                        let r = record_to_reminder(&rec);
                        reminders.insert(r.id.clone(), r);
                    }
                    "Hashtag" => {
                        let h = record_to_hashtag(&rec);
                        out.hashtags.insert(h.id.clone(), h);
                    }
                    "RecurrenceRule" => {
                        let r = record_to_recurrence_rule(&rec);
                        out.recurrence_rules.insert(r.id.clone(), r);
                    }
                    "Alarm" => {
                        let a = record_to_alarm(&rec);
                        out.alarms.insert(a.id.clone(), a);
                    }
                    "AlarmTrigger" => {
                        if let Some(t) = record_to_trigger(&rec) {
                            out.triggers.insert(t.id.clone(), t);
                        }
                    }
                    "Attachment" => {
                        if let Some(a) = record_to_url_attachment(&rec) {
                            out.url_attachments.insert(a.id.clone(), a);
                        }
                    }
                    _ => {}
                }
            }
            continuation = page.continuation;
            if continuation.is_none() {
                break;
            }
        }

        // The query can return records of other lists: keep this list's only.
        out.reminders = reminders.into_values().filter(|r| r.list_id == list_id).collect();
        let reminder_ids: std::collections::BTreeSet<&str> = out.reminders.iter().map(|r| r.id.as_str()).collect();
        out.hashtags.retain(|_, h| reminder_ids.contains(h.reminder_id.as_str()));
        out.recurrence_rules.retain(|_, r| reminder_ids.contains(r.reminder_id.as_str()));
        out.alarms.retain(|_, a| reminder_ids.contains(a.reminder_id.as_str()));
        out.url_attachments.retain(|_, a| reminder_ids.contains(a.reminder_id.as_str()));
        let alarm_ids: std::collections::BTreeSet<String> = out.alarms.keys().cloned().collect();
        out.triggers.retain(|_, t| alarm_ids.contains(&t.alarm_id));
        Ok(out)
    }

    /// One reminder by id (`Reminder/…` or the bare id).
    pub fn get(&self, reminder_id: &str) -> Result<Reminder> {
        let name = record_name(reminder_id, "Reminder");
        let items = records_or_error(self.ck.lookup(&self.zone, std::slice::from_ref(&name))?, "Lookup reminder")?;
        items
            .iter()
            .find_map(|item| match item {
                RecordItem::Record(rec) if rec.record_name == name => Some(record_to_reminder(rec)),
                _ => None,
            })
            .ok_or_else(|| Error::Api { code: Some("NOT_FOUND".into()), reason: format!("reminder not found: {name}") })
    }

    /// A token marking "now" in the zone's history, for
    /// [`changes_since`](Self::changes_since).
    pub fn sync_cursor(&self) -> Result<String> {
        // A one-record query returns the current token as a by-product.
        match self.ck.query(&self.zone, json!({"recordType": "reminderList"}), Some(1), None) {
            Ok(page) => {
                if let Some(token) = page.sync_token {
                    return Ok(token);
                }
            }
            Err(err) if err.is_auth() => return Err(err),
            Err(err) => tracing::debug!("sync token query failed, walking the zone instead: {err}"),
        }
        let (_, token) = self.zone_changes(Some(Vec::new()), Some(Vec::new()), None, Some(false))?;
        token.ok_or_else(|| Error::Protocol("unable to obtain a sync token for the Reminders zone".into()))
    }

    /// Reminders changed since `since` (or all of them when `None`), and the
    /// token to pass next time.
    pub fn changes_since(&self, since: Option<&str>) -> Result<(Vec<ReminderChange>, String)> {
        let (items, token) =
            self.zone_changes(Some(vec!["Reminder".into()]), None, since.map(str::to_owned), Some(false))?;
        let mut changes = Vec::new();
        for item in items {
            match item {
                RecordItem::Record(rec) if rec.record_type == "Reminder" => {
                    let reminder = record_to_reminder(&rec);
                    changes.push(if reminder.deleted {
                        ReminderChange::Deleted { id: reminder.id }
                    } else {
                        ReminderChange::Updated(Box::new(reminder))
                    });
                }
                RecordItem::Record(_) => {}
                RecordItem::Tombstone { record_name } => changes.push(ReminderChange::Deleted { id: record_name }),
                RecordItem::Error { record_name, code, reason } => {
                    return Err(Error::Api {
                        code: Some(code.clone()),
                        reason: format!(
                            "iterating reminder changes failed for {}: {code} ({})",
                            record_name.as_deref().unwrap_or("<unknown record>"),
                            reason.as_deref().unwrap_or("no reason provided")
                        ),
                    });
                }
            }
        }
        let token = token.ok_or_else(|| Error::Protocol("changes/zone returned no sync token".into()))?;
        Ok((changes, token))
    }

    // ---- writes --------------------------------------------------------------

    fn now_ms(&self) -> i64 {
        datetime_to_millis((self.now)())
    }

    /// `{"map": {field: {"counter": 1, "modificationTime": …, "replicaID": …}}}`
    /// with fields in the order given, compact, like pyicloud's.
    fn resolution_token_map(&self, fields: &[&str]) -> Result<String> {
        let now = (self.now)();
        // Seconds as a float the way Python's time.time() holds them, then
        // shifted to Apple's epoch: same arithmetic, same last digit.
        let seconds = now.unix_timestamp() as f64 + f64::from(now.nanosecond()) / 1e9;
        let apple_time = serde_json::to_string(&(seconds - APPLE_EPOCH_OFFSET))?;
        let mut out = String::from("{\"map\":{");
        for (i, field) in fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let replica = (self.new_id)()?;
            let _ = write!(
                out,
                "{}:{{\"counter\":1,\"modificationTime\":{apple_time},\"replicaID\":{}}}",
                serde_json::to_string(field)?,
                serde_json::to_string(&replica)?
            );
        }
        out.push_str("}}");
        Ok(out)
    }

    fn modify(&self, operations: &[Operation], atomic: Option<bool>, what: &str) -> Result<Vec<RecordItem>> {
        records_or_error(self.ck.modify(&self.zone, operations, atomic)?, what)
    }

    fn change_tag(items: &[RecordItem], name: &str) -> Option<String> {
        items.iter().rev().find_map(|item| match item {
            RecordItem::Record(rec) if rec.record_name == name => rec.record_change_tag.clone(),
            _ => None,
        })
    }

    /// Create a reminder and return it as iCloud stored it.
    pub fn create(&self, new: &NewReminder) -> Result<Reminder> {
        let name = format!("Reminder/{}", (self.new_id)()?);
        let title = crdt::encode(&new.title);
        let notes = crdt::encode(&new.notes);
        let token_map = self.resolution_token_map(&[
            "allDay",
            "titleDocument",
            "notesDocument",
            "parentReminder",
            "priority",
            "icsDisplayOrder",
            "creationDate",
            "list",
            "flagged",
            "completed",
            "completionDate",
            "lastModifiedDate",
            "recurrenceRuleIDs",
            "dueDate",
            "timeZone",
        ])?;
        let now = self.now_ms();

        let mut fields = vec![
            ("AllDay".into(), FieldValue::Int(i64::from(new.all_day))),
            ("Completed".into(), FieldValue::Int(i64::from(new.completed))),
            (
                "CompletionDate".into(),
                if new.completed { FieldValue::Timestamp(now) } else { FieldValue::Null("TIMESTAMP") },
            ),
            ("CreationDate".into(), FieldValue::Timestamp(now)),
            ("Deleted".into(), FieldValue::Int(0)),
            ("Flagged".into(), FieldValue::Int(i64::from(new.flagged))),
            ("Imported".into(), FieldValue::Int(0)),
            ("LastModifiedDate".into(), FieldValue::Timestamp(now)),
            ("List".into(), FieldValue::Reference(new.list_id.clone())),
            ("NotesDocument".into(), FieldValue::Str(notes)),
            ("Priority".into(), FieldValue::Int(new.priority)),
            ("ResolutionTokenMap".into(), FieldValue::Str(token_map)),
            ("TitleDocument".into(), FieldValue::Str(title)),
        ];
        if let Some(due) = new.due_date {
            fields.push(("DueDate".into(), FieldValue::Timestamp(datetime_to_millis(due))));
        }
        if let Some(tz) = new.time_zone.as_deref().filter(|s| !s.is_empty()) {
            fields.push(("TimeZone".into(), FieldValue::Str(tz.to_owned())));
        }
        if let Some(parent) = new.parent_reminder_id.as_deref().filter(|s| !s.is_empty()) {
            fields.push(("ParentReminder".into(), FieldValue::Reference(record_name(parent, "Reminder"))));
        }

        let op = Operation {
            kind: OperationType::Create,
            record_name: name.clone(),
            record_type: "Reminder".into(),
            fields,
            record_change_tag: None,
            parent: Some(new.list_id.clone()),
        };
        self.modify(&[op], None, "Create reminder")?;
        self.get(&name)
    }

    /// Write back a reminder's title, notes, completion, priority, flag,
    /// all-day flag, due date, time zone and parent. Its change tag and
    /// modification time are updated in place.
    pub fn update(&self, reminder: &mut Reminder) -> Result<()> {
        let name = record_name(&reminder.id, "Reminder");
        let now = self.now_ms();
        let completed_date = if reminder.completed {
            reminder
                .completed_date
                .or_else(|| OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000).ok())
        } else {
            None
        };

        let fields = vec![
            ("TitleDocument".into(), FieldValue::Str(crdt::encode(&reminder.title))),
            ("NotesDocument".into(), FieldValue::Str(crdt::encode(&reminder.notes))),
            ("Completed".into(), FieldValue::Int(i64::from(reminder.completed))),
            ("CompletionDate".into(), FieldValue::timestamp_or_null(completed_date)),
            ("Priority".into(), FieldValue::Int(reminder.priority)),
            ("Flagged".into(), FieldValue::Int(i64::from(reminder.flagged))),
            ("AllDay".into(), FieldValue::Int(i64::from(reminder.all_day))),
            ("LastModifiedDate".into(), FieldValue::Timestamp(now)),
            ("DueDate".into(), FieldValue::timestamp_or_null(reminder.due_date)),
            ("TimeZone".into(), FieldValue::str_or_null(reminder.time_zone.as_deref())),
            (
                "ParentReminder".into(),
                FieldValue::reference_or_null(
                    reminder.parent_reminder_id.as_deref().map(|p| record_name(p, "Reminder")).as_deref(),
                ),
            ),
        ];
        let token_map = self.resolution_token_map(&[
            "titleDocument",
            "notesDocument",
            "completed",
            "completionDate",
            "priority",
            "flagged",
            "allDay",
            "lastModifiedDate",
            "dueDate",
            "timeZone",
            "parentReminder",
        ])?;
        let mut fields = fields;
        fields.push(("ResolutionTokenMap".into(), FieldValue::Str(token_map)));

        let items = self.modify(
            &[self.update_op(&name, "Reminder", reminder.record_change_tag.clone(), fields)],
            None,
            "Update reminder",
        )?;
        if let Some(tag) = Self::change_tag(&items, &name) {
            reminder.record_change_tag = Some(tag);
        }
        reminder.completed_date = completed_date;
        reminder.modified = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000).ok();
        Ok(())
    }

    /// Soft-delete a reminder (`Deleted = 1`), as icloud.com does.
    pub fn delete(&self, reminder: &mut Reminder) -> Result<()> {
        let name = record_name(&reminder.id, "Reminder");
        let token_map = self.resolution_token_map(&["deleted", "lastModifiedDate"])?;
        let now = self.now_ms();
        let fields = vec![
            ("Deleted".into(), FieldValue::Int(1)),
            ("ResolutionTokenMap".into(), FieldValue::Str(token_map)),
            ("LastModifiedDate".into(), FieldValue::Timestamp(now)),
        ];
        let items = self.modify(
            &[self.update_op(&name, "Reminder", reminder.record_change_tag.clone(), fields)],
            None,
            "Delete reminder",
        )?;
        if let Some(tag) = Self::change_tag(&items, &name) {
            reminder.record_change_tag = Some(tag);
        }
        reminder.deleted = true;
        reminder.modified = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000).ok();
        Ok(())
    }

    fn update_op(
        &self,
        name: &str,
        record_type: &str,
        tag: Option<String>,
        fields: Vec<(String, FieldValue)>,
    ) -> Operation {
        Operation {
            kind: OperationType::Update,
            record_name: name.to_owned(),
            record_type: record_type.to_owned(),
            fields,
            record_change_tag: tag,
            parent: None,
        }
    }

    /// The reminder-side half of linking or unlinking a child record: its
    /// id list, a token for that field and the modification time.
    fn linked_ids_op(
        &self,
        reminder: &Reminder,
        field: &str,
        token_field: &str,
        ids: Vec<String>,
    ) -> Result<Operation> {
        let token_map = self.resolution_token_map(&[token_field, "lastModifiedDate"])?;
        let now = self.now_ms();
        Ok(self.update_op(
            &record_name(&reminder.id, "Reminder"),
            "Reminder",
            reminder.record_change_tag.clone(),
            vec![
                (field.to_owned(), FieldValue::StrList(ids)),
                ("ResolutionTokenMap".into(), FieldValue::Str(token_map)),
                ("LastModifiedDate".into(), FieldValue::Timestamp(now)),
            ],
        ))
    }

    /// Create a child record of `reminder` and add it to the reminder's id
    /// list, atomically. Returns the child's record name and change tag.
    fn create_linked(
        &self,
        reminder: &mut Reminder,
        ids: fn(&mut Reminder) -> &mut Vec<String>,
        (prefix, field, token_field): (&str, &str, &str),
        child_fields: impl FnOnce(&str) -> Vec<(String, FieldValue)>,
        what: &str,
    ) -> Result<(String, Option<String>)> {
        let child_uuid = (self.new_id)()?;
        let child_name = format!("{prefix}/{child_uuid}");
        let reminder_name = record_name(&reminder.id, "Reminder");
        let mut linked: Vec<String> = ids(reminder).iter().map(|id| raw_id(id, prefix)).collect();
        linked.push(child_uuid);

        let reminder_op = self.linked_ids_op(reminder, field, token_field, linked.clone())?;
        let child_op = Operation {
            kind: OperationType::Create,
            record_name: child_name.clone(),
            record_type: prefix.to_owned(),
            fields: child_fields(&reminder_name),
            record_change_tag: None,
            parent: Some(reminder_name.clone()),
        };
        let items = self.modify(&[reminder_op, child_op], Some(true), what)?;
        *ids(reminder) = linked;
        if let Some(tag) = Self::change_tag(&items, &reminder_name) {
            reminder.record_change_tag = Some(tag);
        }
        Ok((child_name.clone(), Self::change_tag(&items, &child_name)))
    }

    /// Soft-delete a child record of `reminder` and remove it from the
    /// reminder's id list, atomically.
    fn delete_linked(
        &self,
        reminder: &mut Reminder,
        ids: fn(&mut Reminder) -> &mut Vec<String>,
        (prefix, field, token_field): (&str, &str, &str),
        child_id: &str,
        child_tag: Option<String>,
        what: &str,
    ) -> Result<()> {
        let child_name = record_name(child_id, prefix);
        let child_uuid = raw_id(&child_name, prefix);
        let reminder_name = record_name(&reminder.id, "Reminder");
        let linked: Vec<String> =
            ids(reminder).iter().map(|id| raw_id(id, prefix)).filter(|id| *id != child_uuid).collect();

        let reminder_op = self.linked_ids_op(reminder, field, token_field, linked.clone())?;
        let child_op = self.update_op(
            &child_name,
            prefix,
            child_tag,
            vec![
                ("Deleted".into(), FieldValue::Int(1)),
                ("Reminder".into(), FieldValue::Reference(reminder_name.clone())),
            ],
        );
        let items = self.modify(&[reminder_op, child_op], Some(true), what)?;
        *ids(reminder) = linked;
        if let Some(tag) = Self::change_tag(&items, &reminder_name) {
            reminder.record_change_tag = Some(tag);
        }
        Ok(())
    }

    /// Tag a reminder.
    pub fn create_hashtag(&self, reminder: &mut Reminder, name: &str) -> Result<Hashtag> {
        let now = self.now_ms();
        let tag_name = name.to_owned();
        let (id, tag) = self.create_linked(
            reminder,
            |r| &mut r.hashtag_ids,
            ("Hashtag", "HashtagIDs", "hashtagIDs"),
            |reminder_name| {
                vec![
                    ("Name".into(), FieldValue::EncryptedText(tag_name)),
                    ("Deleted".into(), FieldValue::Int(0)),
                    ("Reminder".into(), FieldValue::Reference(reminder_name.to_owned())),
                    ("CreationDate".into(), FieldValue::Timestamp(now)),
                ]
            },
            "Create hashtag",
        )?;
        Ok(Hashtag {
            id,
            name: name.to_owned(),
            reminder_id: record_name(&reminder.id, "Reminder"),
            record_change_tag: tag,
        })
    }

    /// Remove a tag from a reminder.
    pub fn delete_hashtag(&self, reminder: &mut Reminder, hashtag: &Hashtag) -> Result<()> {
        self.delete_linked(
            reminder,
            |r| &mut r.hashtag_ids,
            ("Hashtag", "HashtagIDs", "hashtagIDs"),
            &hashtag.id,
            hashtag.record_change_tag.clone(),
            "Delete hashtag",
        )
    }

    /// Make a reminder repeat.
    pub fn create_recurrence_rule(&self, reminder: &mut Reminder, rule: &RecurrenceRule) -> Result<RecurrenceRule> {
        let rule_fields = rule.clone();
        let (id, tag) = self.create_linked(
            reminder,
            |r| &mut r.recurrence_rule_ids,
            ("RecurrenceRule", "RecurrenceRuleIDs", "recurrenceRuleIDs"),
            |reminder_name| {
                vec![
                    ("Reminder".into(), FieldValue::Reference(reminder_name.to_owned())),
                    ("Frequency".into(), FieldValue::Int(rule_fields.frequency.raw())),
                    ("Interval".into(), FieldValue::Int(i64::from(rule_fields.interval.max(1)))),
                    ("OccurrenceCount".into(), FieldValue::Int(i64::from(rule_fields.occurrence_count))),
                    ("FirstDayOfTheWeek".into(), FieldValue::Int(i64::from(rule_fields.first_day_of_week.min(6)))),
                    ("Imported".into(), FieldValue::Int(0)),
                    ("Deleted".into(), FieldValue::Int(0)),
                ]
            },
            "Create recurrence rule",
        )?;
        Ok(RecurrenceRule {
            id,
            reminder_id: record_name(&reminder.id, "Reminder"),
            interval: rule.interval.max(1),
            first_day_of_week: rule.first_day_of_week.min(6),
            record_change_tag: tag,
            ..rule.clone()
        })
    }

    /// Write back every field of a recurrence rule.
    pub fn update_recurrence_rule(&self, rule: &mut RecurrenceRule) -> Result<()> {
        let name = record_name(&rule.id, "RecurrenceRule");
        let mut fields = vec![
            ("Frequency".into(), FieldValue::Int(rule.frequency.raw())),
            ("Interval".into(), FieldValue::Int(i64::from(rule.interval.max(1)))),
            ("OccurrenceCount".into(), FieldValue::Int(i64::from(rule.occurrence_count))),
            ("FirstDayOfTheWeek".into(), FieldValue::Int(i64::from(rule.first_day_of_week.min(6)))),
        ];
        if !rule.reminder_id.is_empty() {
            fields.push(("Reminder".into(), FieldValue::Reference(record_name(&rule.reminder_id, "Reminder"))));
        }
        let items = self.modify(
            &[self.update_op(&name, "RecurrenceRule", rule.record_change_tag.clone(), fields)],
            None,
            "Update recurrence rule",
        )?;
        if let Some(tag) = Self::change_tag(&items, &name) {
            rule.record_change_tag = Some(tag);
        }
        Ok(())
    }

    /// Stop a reminder from repeating.
    pub fn delete_recurrence_rule(&self, reminder: &mut Reminder, rule: &RecurrenceRule) -> Result<()> {
        self.delete_linked(
            reminder,
            |r| &mut r.recurrence_rule_ids,
            ("RecurrenceRule", "RecurrenceRuleIDs", "recurrenceRuleIDs"),
            &rule.id,
            rule.record_change_tag.clone(),
            "Delete recurrence rule",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_convert_between_both_forms() {
        assert_eq!(record_name("ABC", "Reminder"), "Reminder/ABC");
        assert_eq!(record_name("Reminder/ABC", "Reminder"), "Reminder/ABC");
        assert_eq!(raw_id("Alarm/X1", "Alarm"), "X1");
        assert_eq!(raw_id("X1", "Alarm"), "X1");
    }

    #[test]
    fn url_attachments_may_be_base64() {
        assert_eq!(decode_attachment_url("https://example.com/a"), "https://example.com/a");
        assert_eq!(decode_attachment_url("aHR0cHM6Ly9leGFtcGxlLmNvbS9h"), "https://example.com/a");
        assert_eq!(decode_attachment_url("not a url"), "not a url");
    }

    #[test]
    fn reminder_id_lists_must_be_string_arrays() {
        assert_eq!(parse_reminder_ids(r#"["Reminder/A","B"]"#, "x").unwrap(), vec!["A", "B"]);
        assert!(parse_reminder_ids(r#"{"a":1}"#, "x").is_err());
        assert!(parse_reminder_ids(r"[1]", "x").is_err());
    }
}
