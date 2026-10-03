//! Print the VTODO this crate writes for a sample reminder, or, given a file,
//! read a VTODO and print the task it maps to.
//!
//! ```text
//! cargo run -p icloud-tasks --example vtodo_sample            # write
//! cargo run -p icloud-tasks --example vtodo_sample -- x.ics   # read
//! ```

use std::collections::BTreeMap;

use icloud_api::reminders::{Frequency, Hashtag, ListContents, RecurrenceRule, Reminder};
use icloud_tasks::Task;
use time::macros::datetime;

fn main() {
    if let Some(path) = std::env::args().nth(1) {
        let text = std::fs::read_to_string(path).expect("readable file");
        println!("{:#?}", Task::from_ics(&text).expect("a VTODO"));
        return;
    }
    let reminder = Reminder {
        id: "Reminder/5B6E0C1A-0000-4000-8000-000000000001".into(),
        list_id: "List/L1".into(),
        title: "Comprare il latte, quello buono 🥛 — una riga abbastanza lunga da dover essere piegata".into(),
        notes: "Intero; 2 litri\nal mercato".into(),
        due_date: Some(datetime!(2026-09-23 08:00 UTC)),
        priority: 1,
        flagged: true,
        time_zone: Some("Europe/Rome".into()),
        hashtag_ids: vec!["H1".into()],
        recurrence_rule_ids: vec!["R1".into()],
        parent_reminder_id: Some("Reminder/P0".into()),
        created: Some(datetime!(2026-05-28 20:26:40 UTC)),
        modified: Some(datetime!(2026-09-10 00:26:40 UTC)),
        ..Reminder::default()
    };
    let contents = ListContents {
        hashtags: BTreeMap::from([(
            "Hashtag/H1".into(),
            Hashtag { id: "Hashtag/H1".into(), name: "spesa, casa".into(), ..Default::default() },
        )]),
        recurrence_rules: BTreeMap::from([(
            "RecurrenceRule/R1".into(),
            RecurrenceRule {
                id: "RecurrenceRule/R1".into(),
                frequency: Frequency::Weekly,
                interval: 2,
                ..Default::default()
            },
        )]),
        ..ListContents::default()
    };
    print!("{}", Task::from_reminder(&reminder, &contents).to_ics());
}
