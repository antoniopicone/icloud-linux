//! The Reminders port replays a session recorded from pyicloud 2.7.0.
//!
//! `fixtures/pyicloud_reminders.json` was produced by
//! `tools/capture_pyicloud_reminders.py`: pyicloud's `RemindersService` ran
//! against a fake session with a frozen clock and sequential UUIDs, and every
//! request (path, query, JSON body) was recorded with the reply it got.
//!
//! Here each recorded request becomes a mock that only answers that exact
//! request. The same calls are made through the Rust service with the same
//! clock and ids, so every mock must be hit once: any difference in a path,
//! a query parameter or a single field of a body leaves a mock unmatched.

use std::{
    collections::BTreeMap,
    io::Read as _,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::Engine as _;
use httpmock::prelude::*;
use icloud_api::{
    Session,
    cloudkit::{CloudKit, Database},
    reminders::{CONTAINER, NewReminder, ReminderChange, RemindersService},
};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339, macros::datetime};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/pyicloud_reminders.json")).unwrap()
}

/// pyicloud's frozen clock: 1 790 000 000.123 s.
fn frozen_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(1_790_000_000_123_000_000).unwrap()
}

fn service(server: &MockServer) -> RemindersService {
    let session = Session::open("user@example.com", None, &server.base_url(), "CID").unwrap();
    let params = BTreeMap::from([
        ("clientBuildNumber".to_owned(), "B".to_owned()),
        ("clientMasteringNumber".to_owned(), "M".to_owned()),
        ("clientId".to_owned(), "CID".to_owned()),
        ("dsid".to_owned(), "42".to_owned()),
    ]);
    let ck = CloudKit::new(session, &server.base_url(), CONTAINER, Database::Private, params);
    let counter = Arc::new(AtomicU64::new(1));
    RemindersService::with_clock_and_ids(ck, frozen_now, move || {
        Ok(format!("00000000-0000-4000-8000-{:012X}", counter.fetch_add(1, Ordering::SeqCst)))
    })
}

/// Replace each CRDT document by the hex of its inflated bytes. The protobuf
/// inside must be identical; the zlib framing around it legitimately differs
/// between Python's zlib and Rust's encoder, and Apple inflates both alike.
fn normalize(mut value: Value) -> Value {
    fn walk(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    if matches!(key.as_str(), "TitleDocument" | "NotesDocument")
                        && let Some(Value::String(wire)) = child.get("value")
                    {
                        let compressed = base64::engine::general_purpose::STANDARD.decode(wire).unwrap();
                        let mut doc = Vec::new();
                        flate2::read::ZlibDecoder::new(compressed.as_slice()).read_to_end(&mut doc).unwrap();
                        child["value"] = Value::String(hex(&doc));
                    } else {
                        walk(child);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut value);
    value
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

fn rfc3339(value: Option<OffsetDateTime>) -> Value {
    value.map_or(Value::Null, |d| Value::String(d.format(&Rfc3339).unwrap()))
}

#[test]
fn every_request_matches_pyicloud_and_the_models_agree() {
    let fixture = fixture();
    let server = MockServer::start();
    let mocks: Vec<_> = fixture["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| {
            let expected = normalize(call["body"].clone());
            server.mock(|mut when, then| {
                when = when.method(POST).path(call["path"].as_str().unwrap()).is_true(move |req| {
                    serde_json::from_slice(&req.body().to_vec()).map(normalize).ok().as_ref() == Some(&expected)
                });
                for (key, value) in call["query"].as_object().unwrap() {
                    when = when.query_param(key, value.as_str().unwrap());
                }
                let _ = when;
                then.status(200).header("content-type", "application/json").json_body(call["reply"].clone());
            })
        })
        .collect();

    let svc = service(&server);

    // Lists.
    let lists = svc.lists().unwrap();
    let expected = &fixture["lists"][0];
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].id, expected["id"].as_str().unwrap());
    assert_eq!(lists[0].title, expected["title"].as_str().unwrap());
    assert_eq!(lists[0].color.as_deref(), expected["color"].as_str());
    assert_eq!(lists[0].count, usize::try_from(expected["count"].as_u64().unwrap()).unwrap());
    assert_eq!(serde_json::to_value(&lists[0].reminder_ids).unwrap(), expected["reminder_ids"]);

    // A list's reminders.
    let contents = svc.list_contents("List/L1").unwrap();
    let expected = &fixture["list_reminders"][0];
    let r = &contents.reminders[0];
    assert_eq!(contents.reminders.len(), 1);
    assert_eq!(r.id, expected["id"].as_str().unwrap());
    assert_eq!(r.list_id, expected["list_id"].as_str().unwrap());
    assert_eq!(r.title, expected["title"].as_str().unwrap());
    assert_eq!(r.notes, expected["desc"].as_str().unwrap());
    assert_eq!(r.completed, expected["completed"].as_bool().unwrap());
    assert_eq!(rfc3339(r.completed_date), expected["completed_date"], "the 0001-01-01 sentinel reads as unset");
    assert_eq!(rfc3339(r.due_date), expected["due_date"]);
    assert_eq!(rfc3339(r.created), expected["created"]);
    assert_eq!(rfc3339(r.modified), expected["modified"]);
    assert_eq!(r.priority, expected["priority"].as_i64().unwrap());
    assert_eq!(r.flagged, expected["flagged"].as_bool().unwrap());
    assert_eq!(r.all_day, expected["all_day"].as_bool().unwrap());
    assert_eq!(r.time_zone.as_deref(), expected["time_zone"].as_str());
    assert_eq!(serde_json::to_value(&r.alarm_ids).unwrap(), expected["alarm_ids"]);
    assert_eq!(serde_json::to_value(&r.hashtag_ids).unwrap(), expected["hashtag_ids"]);
    assert_eq!(r.parent_reminder_id.as_deref(), expected["parent_reminder_id"].as_str());
    assert_eq!(r.record_change_tag.as_deref(), expected["record_change_tag"].as_str());

    // Incremental sync.
    assert_eq!(svc.sync_cursor().unwrap(), fixture["sync_cursor"].as_str().unwrap());
    let (changes, token) = svc.changes_since(Some("TOK-1")).unwrap();
    assert_eq!(token, "TOK-2");
    let expected = fixture["changes"].as_array().unwrap();
    assert_eq!(changes.len(), expected.len());
    assert!(matches!(&changes[0], ReminderChange::Updated(r) if r.id == expected[0]["reminder_id"].as_str().unwrap()));
    assert!(
        matches!(&changes[1], ReminderChange::Deleted { id } if id == expected[1]["reminder_id"].as_str().unwrap())
    );

    // Writes: create, then update and delete an existing reminder.
    let created = svc
        .create(&NewReminder {
            list_id: "List/L1".into(),
            title: "Comprare il latte".into(),
            notes: "Intero, 2 litri".into(),
            due_date: Some(datetime!(2026-09-23 10:00 UTC)),
            priority: 1,
            flagged: true,
            time_zone: Some("Europe/Rome".into()),
            parent_reminder_id: Some("P0".into()),
            ..NewReminder::default()
        })
        .unwrap();
    assert_eq!(created.id, fixture["created_id"].as_str().unwrap());

    let mut rem = svc.get("Reminder/AAAA-1").unwrap();
    rem.title = "Comprare il latte di soia".into();
    rem.completed = true;
    rem.due_date = None;
    rem.parent_reminder_id = None;
    svc.update(&mut rem).unwrap();
    assert_eq!(rem.record_change_tag.as_deref(), Some("tag-NEW"), "the new change tag is adopted");
    assert_eq!(rem.completed_date, Some(frozen_now()), "completion is stamped with the current time");
    svc.delete(&mut rem).unwrap();
    assert!(rem.deleted);

    for (i, mock) in mocks.iter().enumerate() {
        assert_eq!(mock.calls(), 1, "recorded call #{i} ({}) was not reproduced exactly", fixture["calls"][i]["path"]);
    }
}

#[test]
fn a_per_record_error_on_write_is_reported_with_the_record_name() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path_includes("records/modify");
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "records": [{"recordName": "Reminder/AAAA-1", "serverErrorCode": "CONFLICT", "reason": "oplock error"}]
        }));
    });
    let svc = service(&server);
    let mut rem = icloud_api::reminders::Reminder { id: "Reminder/AAAA-1".into(), ..Default::default() };
    let err = svc.update(&mut rem).unwrap_err().to_string();
    assert!(err.contains("Reminder/AAAA-1: CONFLICT (oplock error)"), "{err}");
    assert_eq!(rem.record_change_tag, None, "nothing is adopted from a failed write");
}

#[test]
fn an_expired_session_is_an_auth_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path_includes("changes/zone");
        then.status(401)
            .header("content-type", "application/json")
            .json_body(serde_json::json!({"serverErrorCode": "AUTHENTICATION_FAILED", "reason": "no auth"}));
    });
    let err = service(&server).lists().unwrap_err();
    assert!(err.is_auth(), "an expired CloudKit session must ask for a new sign-in: {err:?}");
}
