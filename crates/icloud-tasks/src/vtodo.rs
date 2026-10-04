//! iCloud reminders ⇄ iCalendar `VTODO`.
//!
//! | Reminders                      | VTODO                                        |
//! |--------------------------------|----------------------------------------------|
//! | record name `Reminder/<id>`    | `UID:<id>`                                   |
//! | title, notes                   | `SUMMARY`, `DESCRIPTION`                     |
//! | completed, completion date     | `STATUS:COMPLETED`, `COMPLETED`, `PERCENT-COMPLETE:100` |
//! | due date                       | `DUE` in UTC, or `DUE;VALUE=DATE` if all-day |
//! | time zone                      | `X-ICLOUD-TIMEZONE`                          |
//! | priority (0/1/5/9)             | `PRIORITY` (same scale)                      |
//! | hashtags                       | `CATEGORIES`                                 |
//! | recurrence rule                | `RRULE:FREQ=…;INTERVAL=…;COUNT=…`            |
//! | parent reminder                | `RELATED-TO;RELTYPE=PARENT`                  |
//! | flagged                        | `X-ICLOUD-FLAGGED:1`                         |
//! | URL attachment                 | `URL`                                        |
//! | location alarms                | `X-ICLOUD-LOCATION` (kept, never written back) |
//! | timed due date                 | a `VALARM` at the due time                   |
//!
//! What Reminders cannot hold is reported rather than silently dropped:
//! [`Task::lossy`] lists RRULE parts beyond frequency, interval and count.
//! Other clients' own properties (`X-APPLE-SORT-ORDER`, …) are kept in
//! [`Task::extra`] so a round trip through this module does not strip them.
//!
//! All-day due dates are written back as midnight in the reminder's time zone
//! (UTC when it has none). How Apple's own apps store all-day dates has not
//! been checked against a real account yet.

use std::fmt::Write as _;

use icloud_api::reminders::{
    Frequency, Hashtag, ListContents, LocationTrigger, RecurrenceRule, Reminder, raw_id, record_name,
};
use jiff::{Timestamp, civil, tz::TimeZone};
use time::OffsetDateTime;

use crate::{
    Error,
    ical::{self, Component, Property},
};

const PRODID: &str = "-//icloud-linux//icloud-tasks//EN";
/// Properties this module owns. Anything else on a VTODO is passed through.
const OWNED: &[&str] = &[
    "UID",
    "SUMMARY",
    "DESCRIPTION",
    "STATUS",
    "COMPLETED",
    "PERCENT-COMPLETE",
    "DUE",
    "DTSTART",
    "PRIORITY",
    "CATEGORIES",
    "RRULE",
    "RELATED-TO",
    "URL",
    "DTSTAMP",
    "CREATED",
    "LAST-MODIFIED",
    "X-ICLOUD-TIMEZONE",
    "X-ICLOUD-FLAGGED",
    "X-ICLOUD-LOCATION",
    "X-ICLOUD-FIRST-DAY-OF-WEEK",
];

/// When a task is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// All-day.
    Date(civil::Date),
    /// A moment in time.
    At(Timestamp),
}

/// A recurrence Reminders can represent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    pub frequency: Frequency,
    pub interval: u32,
    /// 0 = forever.
    pub count: u32,
    /// Reminders' raw first-day-of-week value, carried through unchanged
    /// (its meaning is not documented, so it is not mapped to `WKST`).
    pub first_day_of_week: u8,
}

/// A task in the shape both sides share.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Task {
    /// The reminder's id without the `Reminder/` prefix.
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub completed: bool,
    pub completed_at: Option<Timestamp>,
    pub due: Option<Due>,
    pub time_zone: Option<String>,
    /// iCalendar scale: 0 none, 1 highest … 9 lowest.
    pub priority: u8,
    pub categories: Vec<String>,
    pub recurrence: Option<Recurrence>,
    pub parent_uid: Option<String>,
    pub flagged: bool,
    pub url: Option<String>,
    pub created: Option<Timestamp>,
    pub modified: Option<Timestamp>,
    /// Location alarms, as JSON, for display only.
    pub locations: Vec<String>,
    /// Properties of other clients, passed through untouched.
    pub extra: Vec<Property>,
    /// What the VTODO said that Reminders cannot store.
    pub lossy: Vec<String>,
}

// ---- conversions between the two time libraries --------------------------------

fn to_jiff(value: OffsetDateTime) -> Option<Timestamp> {
    Timestamp::from_nanosecond(value.unix_timestamp_nanos()).ok()
}

fn to_time(value: Timestamp) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond()).ok()
}

fn zone(name: Option<&str>) -> TimeZone {
    name.and_then(|n| TimeZone::get(n).ok()).unwrap_or(TimeZone::UTC)
}

fn utc_stamp(ts: Timestamp) -> String {
    ts.strftime("%Y%m%dT%H%M%SZ").to_string()
}

// ---- reminder → task ------------------------------------------------------------

impl Task {
    /// Build the task for one reminder of `contents`.
    pub fn from_reminder(reminder: &Reminder, contents: &ListContents) -> Self {
        let mut hashtags: Vec<&Hashtag> =
            reminder.hashtag_ids.iter().filter_map(|id| contents.hashtags.get(&record_name(id, "Hashtag"))).collect();
        hashtags.sort_by(|a, b| a.name.cmp(&b.name));
        let recurrence = reminder
            .recurrence_rule_ids
            .iter()
            .find_map(|id| contents.recurrence_rules.get(&record_name(id, "RecurrenceRule")))
            .map(|rule: &RecurrenceRule| Recurrence {
                frequency: rule.frequency,
                interval: rule.interval.max(1),
                count: rule.occurrence_count,
                first_day_of_week: rule.first_day_of_week,
            });
        let locations = reminder
            .alarm_ids
            .iter()
            .filter_map(|id| contents.alarms.get(&record_name(id, "Alarm")))
            .filter_map(|alarm| contents.triggers.get(&record_name(&alarm.trigger_id, "AlarmTrigger")))
            .map(location_json)
            .collect();
        let url = reminder
            .attachment_ids
            .iter()
            .find_map(|id| contents.url_attachments.get(&record_name(id, "Attachment")))
            .map(|a| a.url.clone());

        let due = reminder.due_date.and_then(to_jiff).map(|ts| {
            if reminder.all_day {
                Due::Date(ts.to_zoned(zone(reminder.time_zone.as_deref())).date())
            } else {
                Due::At(ts)
            }
        });

        Self {
            uid: raw_id(&reminder.id, "Reminder"),
            summary: reminder.title.clone(),
            description: reminder.notes.clone(),
            completed: reminder.completed,
            completed_at: reminder.completed_date.and_then(to_jiff),
            due,
            time_zone: reminder.time_zone.clone(),
            priority: u8::try_from(reminder.priority.clamp(0, 9)).unwrap_or(0),
            categories: hashtags.iter().map(|h| h.name.clone()).collect(),
            recurrence,
            parent_uid: reminder.parent_reminder_id.as_deref().map(|p| raw_id(p, "Reminder")),
            flagged: reminder.flagged,
            url,
            created: reminder.created.and_then(to_jiff),
            modified: reminder.modified.and_then(to_jiff),
            locations,
            extra: Vec::new(),
            lossy: Vec::new(),
        }
    }

    /// The `VTODO` component, without the surrounding calendar.
    pub fn to_component(&self) -> Component {
        let mut todo = Component::new("VTODO");
        let stamp = self.modified.unwrap_or_else(Timestamp::now);
        todo.push(Property::text("UID", &self.uid));
        todo.push(Property::new("DTSTAMP", utc_stamp(stamp)));
        if let Some(created) = self.created {
            todo.push(Property::new("CREATED", utc_stamp(created)));
        }
        todo.push(Property::new("LAST-MODIFIED", utc_stamp(stamp)));
        todo.push(Property::text("SUMMARY", &self.summary));
        if !self.description.is_empty() {
            todo.push(Property::text("DESCRIPTION", &self.description));
        }
        if self.completed {
            todo.push(Property::new("STATUS", "COMPLETED"));
            todo.push(Property::new("PERCENT-COMPLETE", "100"));
            if let Some(at) = self.completed_at {
                todo.push(Property::new("COMPLETED", utc_stamp(at)));
            }
        } else {
            todo.push(Property::new("STATUS", "NEEDS-ACTION"));
        }
        match self.due {
            Some(Due::Date(date)) => {
                todo.push(Property::new("DUE", date.strftime("%Y%m%d").to_string()).with_param("VALUE", "DATE"));
            }
            Some(Due::At(at)) => todo.push(Property::new("DUE", utc_stamp(at))),
            None => {}
        }
        if let Some(tz) = &self.time_zone {
            todo.push(Property::text("X-ICLOUD-TIMEZONE", tz));
        }
        if self.priority > 0 {
            todo.push(Property::new("PRIORITY", self.priority.to_string()));
        }
        if !self.categories.is_empty() {
            let list: Vec<String> = self.categories.iter().map(|c| ical::escape_text(c)).collect();
            todo.push(Property::new("CATEGORIES", list.join(",")));
        }
        if let Some(rule) = &self.recurrence {
            todo.push(Property::new("RRULE", rrule_text(rule)));
            if rule.first_day_of_week != 0 {
                todo.push(Property::new("X-ICLOUD-FIRST-DAY-OF-WEEK", rule.first_day_of_week.to_string()));
            }
        }
        if let Some(parent) = &self.parent_uid {
            todo.push(Property::text("RELATED-TO", parent).with_param("RELTYPE", "PARENT"));
        }
        if self.flagged {
            todo.push(Property::new("X-ICLOUD-FLAGGED", "1"));
        }
        if let Some(url) = &self.url {
            todo.push(Property::new("URL", url.clone()));
        }
        for location in &self.locations {
            todo.push(Property::text("X-ICLOUD-LOCATION", location));
        }
        todo.properties.extend(self.extra.iter().cloned());

        // Reminders alert at the due time of a timed reminder.
        if let (Some(Due::At(at)), false) = (self.due, self.completed) {
            let mut alarm = Component::new("VALARM");
            alarm.push(Property::new("ACTION", "DISPLAY"));
            alarm.push(Property::text("DESCRIPTION", &self.summary));
            alarm.push(Property::new("TRIGGER", utc_stamp(at)).with_param("VALUE", "DATE-TIME"));
            todo.components.push(alarm);
        }
        todo
    }

    /// A complete `VCALENDAR` object holding this task.
    pub fn to_ics(&self) -> String {
        let mut cal = Component::new("VCALENDAR");
        cal.push(Property::new("VERSION", "2.0"));
        cal.push(Property::new("PRODID", PRODID));
        cal.components.push(self.to_component());
        cal.to_ics()
    }
}

fn location_json(t: &LocationTrigger) -> String {
    serde_json::json!({
        "title": t.title, "address": t.address, "latitude": t.latitude, "longitude": t.longitude,
        "radius": t.radius, "proximity": if t.proximity == 2 { "leaving" } else { "arriving" },
    })
    .to_string()
}

fn rrule_text(rule: &Recurrence) -> String {
    let freq = match rule.frequency {
        Frequency::Daily => "DAILY",
        Frequency::Weekly => "WEEKLY",
        Frequency::Monthly => "MONTHLY",
        Frequency::Yearly => "YEARLY",
    };
    let mut out = format!("FREQ={freq}");
    if rule.interval > 1 {
        let _ = write!(out, ";INTERVAL={}", rule.interval);
    }
    if rule.count > 0 {
        let _ = write!(out, ";COUNT={}", rule.count);
    }
    out
}

// ---- task ← iCalendar ---------------------------------------------------------------

impl Task {
    /// Read the first `VTODO` of an iCalendar object.
    pub fn from_ics(input: &str) -> Result<Self, Error> {
        let root = ical::parse(input)?;
        let todo = if root.name == "VTODO" { &root } else { root.component("VTODO").ok_or(Error::NoTodo)? };
        Self::from_component(todo)
    }

    pub fn from_component(todo: &Component) -> Result<Self, Error> {
        let uid = todo.property("UID").map(Property::text_value).filter(|u| !u.is_empty()).ok_or(Error::MissingUid)?;
        let mut task = Self { uid, ..Self::default() };

        task.summary = todo.property("SUMMARY").map(Property::text_value).unwrap_or_default();
        task.description = todo.property("DESCRIPTION").map(Property::text_value).unwrap_or_default();
        let status = todo.property("STATUS").map(|p| p.value.trim().to_ascii_uppercase());
        let percent = todo.property("PERCENT-COMPLETE").and_then(|p| p.value.trim().parse::<u8>().ok());
        task.completed_at = todo.property("COMPLETED").and_then(|p| parse_moment(p).ok());
        task.completed = status.as_deref() == Some("COMPLETED") || percent == Some(100) || task.completed_at.is_some();

        task.time_zone = todo
            .property("X-ICLOUD-TIMEZONE")
            .map(Property::text_value)
            .or_else(|| todo.property("DUE").and_then(|p| p.param("TZID")).map(str::to_owned))
            .filter(|tz| TimeZone::get(tz).is_ok());
        if let Some(due) = todo.property("DUE") {
            task.due = Some(parse_due(due)?);
        }

        task.priority =
            todo.property("PRIORITY").and_then(|p| p.value.trim().parse::<u8>().ok()).map_or(0, |p| p.min(9));
        let mut categories: Vec<String> = todo.properties_named("CATEGORIES").flat_map(Property::text_list).collect();
        categories.sort();
        categories.dedup();
        task.categories = categories;

        if let Some(rrule) = todo.property("RRULE") {
            let first_day =
                todo.property("X-ICLOUD-FIRST-DAY-OF-WEEK").and_then(|p| p.value.trim().parse().ok()).unwrap_or(0);
            let (rule, lossy) = parse_rrule(&rrule.value, first_day);
            task.recurrence = rule;
            task.lossy.extend(lossy);
        }
        task.parent_uid = todo
            .properties_named("RELATED-TO")
            .find(|p| p.param("RELTYPE").is_none_or(|t| t.eq_ignore_ascii_case("PARENT")))
            .map(Property::text_value)
            .filter(|p| !p.is_empty());
        task.flagged = todo.property("X-ICLOUD-FLAGGED").is_some_and(|p| p.value.trim() == "1");
        task.url = todo.property("URL").map(|p| p.value.trim().to_owned()).filter(|u| !u.is_empty());
        task.created = todo.property("CREATED").and_then(|p| parse_moment(p).ok());
        task.modified = todo.property("LAST-MODIFIED").and_then(|p| parse_moment(p).ok());
        task.locations = todo.properties_named("X-ICLOUD-LOCATION").map(Property::text_value).collect();
        task.extra = todo.properties.iter().filter(|p| !OWNED.contains(&p.name.as_str())).cloned().collect();
        Ok(task)
    }
}

fn parse_due(p: &Property) -> Result<Due, Error> {
    let value = p.value.trim();
    if p.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || value.len() == 8 {
        let date = civil::Date::strptime("%Y%m%d", value).map_err(|_| Error::BadDate(value.to_owned()))?;
        return Ok(Due::Date(date));
    }
    parse_moment(p).map(Due::At)
}

/// A DATE-TIME in UTC (`…Z`), with a `TZID`, or floating (taken as local).
fn parse_moment(p: &Property) -> Result<Timestamp, Error> {
    let value = p.value.trim();
    let bad = || Error::BadDate(value.to_owned());
    if let Some(utc) = value.strip_suffix(['Z', 'z']) {
        let dt = civil::DateTime::strptime("%Y%m%dT%H%M%S", utc).map_err(|_| bad())?;
        return dt.to_zoned(TimeZone::UTC).map(|z| z.timestamp()).map_err(|_| bad());
    }
    if value.len() == 8 {
        // A bare date where a moment was expected: midnight UTC.
        let date = civil::Date::strptime("%Y%m%d", value).map_err(|_| bad())?;
        return date.to_zoned(TimeZone::UTC).map(|z| z.timestamp()).map_err(|_| bad());
    }
    let dt = civil::DateTime::strptime("%Y%m%dT%H%M%S", value).map_err(|_| bad())?;
    let tz = match p.param("TZID") {
        Some(name) => TimeZone::get(name.trim_start_matches('/')).unwrap_or_else(|_| {
            tracing::warn!("unknown time zone {name:?}, reading {value} as local time");
            TimeZone::system()
        }),
        None => TimeZone::system(),
    };
    dt.to_zoned(tz).map(|z| z.timestamp()).map_err(|_| bad())
}

/// Frequency, interval and count; anything else is reported as lossy.
fn parse_rrule(text: &str, first_day_of_week: u8) -> (Option<Recurrence>, Vec<String>) {
    let mut frequency = None;
    let mut interval = 1;
    let mut count = 0;
    let mut lossy = Vec::new();
    for part in text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        match key.to_ascii_uppercase().as_str() {
            "FREQ" => {
                frequency = match value.to_ascii_uppercase().as_str() {
                    "DAILY" => Some(Frequency::Daily),
                    "WEEKLY" => Some(Frequency::Weekly),
                    "MONTHLY" => Some(Frequency::Monthly),
                    "YEARLY" => Some(Frequency::Yearly),
                    other => {
                        lossy.push(format!("RRULE frequency {other} is not supported by Reminders"));
                        None
                    }
                };
            }
            "INTERVAL" => interval = value.parse().unwrap_or(1).max(1),
            "COUNT" => count = value.parse().unwrap_or(0),
            "WKST" => {}
            _ => lossy.push(format!("RRULE part {part} is not supported by Reminders")),
        }
    }
    (frequency.map(|frequency| Recurrence { frequency, interval, count, first_day_of_week }), lossy)
}

// ---- task → reminder --------------------------------------------------------------------

/// What [`Task::apply_to`] changed beyond the reminder's own fields.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LinkedChanges {
    pub hashtags_to_add: Vec<String>,
    /// Record names of hashtags to remove.
    pub hashtags_to_remove: Vec<String>,
    pub recurrence: RecurrenceChange,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum RecurrenceChange {
    #[default]
    Unchanged,
    Create(Recurrence),
    /// Record name of the existing rule and its new values.
    Update(String, Recurrence),
    /// Record name of the rule to delete.
    Delete(String),
}

/// iCalendar carries whole seconds, iCloud milliseconds: a value that lands
/// in the same second as the current one is the current one, not an edit.
fn keep_if_same_second(new: Option<OffsetDateTime>, current: Option<OffsetDateTime>) -> Option<OffsetDateTime> {
    match (new, current) {
        (Some(n), Some(c)) if n.unix_timestamp() == c.unix_timestamp() => Some(c),
        _ => new,
    }
}

/// iCalendar priority to the three levels Reminders shows.
fn reminders_priority(priority: u8) -> i64 {
    match priority {
        0 => 0,
        1..=4 => 1,
        5 => 5,
        _ => 9,
    }
}

impl Task {
    /// Copy the task onto `reminder`. Returns whether the reminder's own
    /// fields changed (so an update is needed) and the changes to make to
    /// its linked hashtags and recurrence rule.
    pub fn apply_to(&self, reminder: &mut Reminder, contents: &ListContents) -> (bool, LinkedChanges) {
        let before = reminder.clone();

        reminder.title.clone_from(&self.summary);
        reminder.notes.clone_from(&self.description);
        reminder.completed = self.completed;
        reminder.completed_date = if self.completed {
            keep_if_same_second(self.completed_at.and_then(to_time), before.completed_date).or(before.completed_date)
        } else {
            None
        };
        reminder.priority = reminders_priority(self.priority);
        reminder.flagged = self.flagged;
        if self.time_zone.is_some() {
            reminder.time_zone.clone_from(&self.time_zone);
        }
        match self.due {
            None => {
                reminder.due_date = None;
                reminder.all_day = false;
            }
            Some(Due::At(at)) => {
                reminder.due_date = keep_if_same_second(to_time(at), before.due_date);
                reminder.all_day = false;
            }
            Some(Due::Date(date)) => {
                let tz = zone(reminder.time_zone.as_deref());
                let same_day = before.all_day
                    && before.due_date.and_then(to_jiff).map(|ts| ts.to_zoned(tz.clone()).date()) == Some(date);
                reminder.due_date = if same_day {
                    // Whatever time of day Apple stores for this day, keep it.
                    before.due_date
                } else {
                    date.to_zoned(tz).ok().map(|z| z.timestamp()).and_then(to_time)
                };
                reminder.all_day = true;
            }
        }
        reminder.parent_reminder_id = self.parent_uid.as_deref().map(|p| record_name(p, "Reminder"));

        let changed = reminder.title != before.title
            || reminder.notes != before.notes
            || reminder.completed != before.completed
            || reminder.completed_date != before.completed_date
            || reminder.priority != before.priority
            || reminder.flagged != before.flagged
            || reminder.time_zone != before.time_zone
            || reminder.due_date != before.due_date
            || reminder.all_day != before.all_day
            || reminder.parent_reminder_id != before.parent_reminder_id;

        let current = Self::from_reminder(&before, contents);
        let mut linked = LinkedChanges::default();
        for name in &self.categories {
            if !current.categories.contains(name) {
                linked.hashtags_to_add.push(name.clone());
            }
        }
        for id in &before.hashtag_ids {
            let name = record_name(id, "Hashtag");
            if let Some(tag) = contents.hashtags.get(&name)
                && !self.categories.contains(&tag.name)
            {
                linked.hashtags_to_remove.push(name);
            }
        }
        let existing = before
            .recurrence_rule_ids
            .iter()
            .map(|id| record_name(id, "RecurrenceRule"))
            .find(|name| contents.recurrence_rules.contains_key(name));
        linked.recurrence = match (&self.recurrence, existing) {
            (None, Some(id)) => RecurrenceChange::Delete(id),
            (Some(rule), None) => RecurrenceChange::Create(rule.clone()),
            (Some(rule), Some(id)) if current.recurrence.as_ref() != Some(rule) => {
                RecurrenceChange::Update(id, rule.clone())
            }
            (None, None) | (Some(_), Some(_)) => RecurrenceChange::Unchanged,
        };
        (changed, linked)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use icloud_api::reminders::{Alarm, UrlAttachment};
    use time::macros::datetime;

    use super::*;

    fn sample() -> (Reminder, ListContents) {
        let reminder = Reminder {
            id: "Reminder/AAAA-1".into(),
            list_id: "List/L1".into(),
            title: "Comprare il latte, quello buono".into(),
            notes: "Intero; 2 litri\nal mercato".into(),
            due_date: Some(datetime!(2026-09-23 08:00 UTC)),
            priority: 1,
            flagged: true,
            time_zone: Some("Europe/Rome".into()),
            hashtag_ids: vec!["H1".into(), "H2".into()],
            recurrence_rule_ids: vec!["R1".into()],
            alarm_ids: vec!["A1".into()],
            attachment_ids: vec!["U1".into()],
            parent_reminder_id: Some("Reminder/P0".into()),
            created: Some(datetime!(2026-05-28 20:26:40 UTC)),
            modified: Some(datetime!(2026-09-10 00:26:40 UTC)),
            ..Reminder::default()
        };
        let contents = ListContents {
            reminders: vec![reminder.clone()],
            hashtags: BTreeMap::from([
                ("Hashtag/H1".into(), Hashtag { id: "Hashtag/H1".into(), name: "spesa".into(), ..Default::default() }),
                ("Hashtag/H2".into(), Hashtag { id: "Hashtag/H2".into(), name: "casa".into(), ..Default::default() }),
            ]),
            recurrence_rules: BTreeMap::from([(
                "RecurrenceRule/R1".into(),
                RecurrenceRule {
                    id: "RecurrenceRule/R1".into(),
                    frequency: Frequency::Weekly,
                    interval: 2,
                    occurrence_count: 0,
                    ..Default::default()
                },
            )]),
            alarms: BTreeMap::from([(
                "Alarm/A1".into(),
                Alarm { id: "Alarm/A1".into(), trigger_id: "T1".into(), ..Default::default() },
            )]),
            triggers: BTreeMap::from([(
                "AlarmTrigger/T1".into(),
                LocationTrigger {
                    id: "AlarmTrigger/T1".into(),
                    alarm_id: "Alarm/A1".into(),
                    title: "Casa".into(),
                    ..Default::default()
                },
            )]),
            url_attachments: BTreeMap::from([(
                "Attachment/U1".into(),
                UrlAttachment {
                    id: "Attachment/U1".into(),
                    url: "https://example.com/latte".into(),
                    ..Default::default()
                },
            )]),
        };
        (reminder, contents)
    }

    #[test]
    fn a_reminder_becomes_a_complete_vtodo() {
        let (reminder, contents) = sample();
        let ics = Task::from_reminder(&reminder, &contents).to_ics();
        for line in [
            "UID:AAAA-1",
            "SUMMARY:Comprare il latte\\, quello buono",
            "DESCRIPTION:Intero\\; 2 litri\\nal mercato",
            "STATUS:NEEDS-ACTION",
            "DUE:20260923T080000Z",
            "PRIORITY:1",
            "CATEGORIES:casa,spesa",
            "RRULE:FREQ=WEEKLY;INTERVAL=2",
            "RELATED-TO;RELTYPE=PARENT:P0",
            "X-ICLOUD-FLAGGED:1",
            "X-ICLOUD-TIMEZONE:Europe/Rome",
            "URL:https://example.com/latte",
            "TRIGGER;VALUE=DATE-TIME:20260923T080000Z",
            "CREATED:20260528T202640Z",
        ] {
            assert!(ics.contains(&format!("{line}\r\n")), "missing {line:?} in\n{ics}");
        }
        assert!(ics.contains("X-ICLOUD-LOCATION:"), "location alarm kept for display");
    }

    #[test]
    fn a_vtodo_round_trips_to_the_same_task() {
        let (reminder, contents) = sample();
        let task = Task::from_reminder(&reminder, &contents);
        let back = Task::from_ics(&task.to_ics()).unwrap();
        assert_eq!(back.uid, task.uid);
        assert_eq!(back.summary, task.summary);
        assert_eq!(back.description, task.description);
        assert_eq!(back.due, task.due);
        assert_eq!(back.priority, task.priority);
        assert_eq!(back.categories, task.categories);
        assert_eq!(back.recurrence, task.recurrence);
        assert_eq!(back.parent_uid, task.parent_uid);
        assert_eq!(back.flagged, task.flagged);
        assert_eq!(back.url, task.url);
        assert_eq!(back.time_zone, task.time_zone);
        assert!(back.extra.is_empty() && back.lossy.is_empty());

        // And applying it back to the reminder changes nothing.
        let mut copy = reminder.clone();
        let (changed, linked) = back.apply_to(&mut copy, &contents);
        assert!(!changed, "{copy:#?}");
        assert_eq!(linked, LinkedChanges::default());
    }

    #[test]
    fn all_day_dates_are_days_in_the_reminder_time_zone() {
        let (mut reminder, contents) = sample();
        // 22:30 UTC on the 22nd is already 00:30 on the 23rd in Rome.
        reminder.due_date = Some(datetime!(2026-09-22 22:30 UTC));
        reminder.all_day = true;
        let ics = Task::from_reminder(&reminder, &contents).to_ics();
        assert!(ics.contains("DUE;VALUE=DATE:20260923\r\n"), "{ics}");
        assert!(!ics.contains("BEGIN:VALARM"), "no alarm on all-day reminders");

        // A new all-day date lands on midnight in the reminder's zone.
        let mut fresh = Reminder { time_zone: Some("Europe/Rome".into()), ..Reminder::default() };
        Task::from_ics(&ics).unwrap().apply_to(&mut fresh, &contents);
        assert_eq!(fresh.due_date, Some(datetime!(2026-09-22 22:00 UTC)), "midnight in Rome");
        assert!(fresh.all_day);
    }

    /// The kind of VTODO a `CalDAV` client such as Planify writes.
    #[test]
    fn a_client_written_vtodo_maps_onto_a_reminder() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Planify//EN\r\nBEGIN:VTODO\r\n\
            UID:AAAA-1\r\nSUMMARY:Latte di soia\r\nSTATUS:COMPLETED\r\nCOMPLETED:20260924T090000Z\r\n\
            DUE;TZID=Europe/Rome:20260925T183000\r\nPRIORITY:3\r\nCATEGORIES:casa\r\n\
            CATEGORIES:urgente\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE\r\nX-APPLE-SORT-ORDER:12\r\n\
            END:VTODO\r\nEND:VCALENDAR\r\n";
        let task = Task::from_ics(ics).unwrap();
        assert_eq!(task.extra, vec![Property::new("X-APPLE-SORT-ORDER", "12")]);
        assert_eq!(task.lossy.len(), 1, "BYDAY cannot be stored: {:?}", task.lossy);

        let (mut reminder, contents) = sample();
        let (changed, linked) = task.apply_to(&mut reminder, &contents);
        assert!(changed);
        assert_eq!(reminder.title, "Latte di soia");
        assert_eq!(reminder.notes, "");
        assert!(reminder.completed);
        assert_eq!(reminder.completed_date, Some(datetime!(2026-09-24 09:00 UTC)));
        assert_eq!(reminder.due_date, Some(datetime!(2026-09-25 16:30 UTC)), "18:30 in Rome");
        assert_eq!(reminder.priority, 1, "priority 3 is 'high' in Reminders");
        assert!(!reminder.flagged);
        assert_eq!(reminder.parent_reminder_id, None);
        assert_eq!(linked.hashtags_to_add, vec!["urgente"]);
        assert_eq!(linked.hashtags_to_remove, vec!["Hashtag/H1"], "spesa was removed");
        assert_eq!(
            linked.recurrence,
            RecurrenceChange::Update(
                "RecurrenceRule/R1".into(),
                Recurrence { frequency: Frequency::Weekly, interval: 1, count: 0, first_day_of_week: 0 }
            )
        );

        // Other clients' properties survive our rewrite.
        assert!(task.to_ics().contains("X-APPLE-SORT-ORDER:12\r\n"));
    }

    /// iCloud keeps milliseconds and may store an all-day date at any time
    /// of that day; a VTODO carries neither. Reading a task back must not
    /// look like an edit, or every sync would rewrite every reminder.
    #[test]
    fn precision_lost_in_icalendar_is_not_an_edit() {
        let (mut reminder, contents) = sample();
        reminder.completed = true;
        reminder.completed_date = Some(datetime!(2026-09-24 09:00:00.734 UTC));
        reminder.due_date = Some(datetime!(2026-09-23 08:00:00.250 UTC));
        let back = Task::from_ics(&Task::from_reminder(&reminder, &contents).to_ics()).unwrap();
        let mut copy = reminder.clone();
        let (changed, _) = back.apply_to(&mut copy, &contents);
        assert!(!changed, "{copy:#?}");
        assert_eq!(copy.completed_date, reminder.completed_date, "milliseconds kept");

        // All-day, stored at noon UTC rather than midnight in Rome.
        reminder.all_day = true;
        reminder.due_date = Some(datetime!(2026-09-23 12:00 UTC));
        let back = Task::from_ics(&Task::from_reminder(&reminder, &contents).to_ics()).unwrap();
        let mut copy = reminder.clone();
        let (changed, _) = back.apply_to(&mut copy, &contents);
        assert!(!changed, "same day, different storage time: {copy:#?}");
        assert_eq!(copy.due_date, reminder.due_date);

        // A real move to another day is still an edit.
        let moved = Task { due: Some(Due::Date(civil::date(2026, 9, 24))), ..back };
        let (changed, _) = moved.apply_to(&mut copy, &contents);
        assert!(changed);
        assert_eq!(copy.due_date, Some(datetime!(2026-09-23 22:00 UTC)), "midnight of the 24th in Rome");
    }

    #[test]
    fn removing_due_date_and_recurrence_clears_them() {
        let (mut reminder, contents) = sample();
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:AAAA-1\r\nSUMMARY:x\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (changed, linked) = Task::from_ics(ics).unwrap().apply_to(&mut reminder, &contents);
        assert!(changed);
        assert_eq!(reminder.due_date, None);
        assert_eq!(linked.recurrence, RecurrenceChange::Delete("RecurrenceRule/R1".into()));
        assert_eq!(linked.hashtags_to_remove.len(), 2);
    }

    #[test]
    fn a_vtodo_without_uid_is_refused() {
        assert!(matches!(
            Task::from_ics("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nSUMMARY:x\r\nEND:VTODO\r\nEND:VCALENDAR\r\n"),
            Err(Error::MissingUid)
        ));
        assert!(matches!(Task::from_ics("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"), Err(Error::NoTodo)));
    }
}
