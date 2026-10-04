//! Read-only probe of the Reminders port against a real iCloud account.
//!
//! ```text
//! cargo run -p icloud-tasks --example reminders_probe -- you@icloud.com
//!     [--session-dir DIR]   where to keep the session (default ~/.icloud-reminders-probe)
//!     [--sms]               ask for the verification code by text message
//!     [--show-titles]       also print list names and the reminders that fail a check
//!     [--ics DIR]           write every reminder as a .ics file into DIR
//! ```
//!
//! It only reads: lists, each list's contents, the sync cursor and the full
//! change feed. Nothing is created, changed or deleted in iCloud. Signing in
//! does mark this session as trusted, as any sign-in with a code does.
//!
//! By default the output holds counts only, no names or titles, so it can be
//! shared. Every reminder is also pushed through the VTODO mapping and back;
//! a reminder that comes back different is a bug in the mapping.

use std::{
    collections::BTreeMap,
    io::{self, BufRead as _, Write as _},
    path::PathBuf,
    time::Instant,
};

use icloud_api::{
    Client, ClientConfig, CodeMethod, Error, LoginStatus, TrustedPhone,
    reminders::{ListContents, Reminder, ReminderChange},
};
use icloud_tasks::{LinkedChanges, RecurrenceChange, Task};
use secrecy::SecretString;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Options {
    account: String,
    session_dir: PathBuf,
    sms: bool,
    show_titles: bool,
    ics_dir: Option<PathBuf>,
}

fn options() -> Result<Options> {
    let mut args = std::env::args().skip(1);
    let mut account = None;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut opts = Options {
        account: String::new(),
        session_dir: home.join(".icloud-reminders-probe"),
        sms: false,
        show_titles: false,
        ics_dir: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session-dir" => opts.session_dir = args.next().ok_or("--session-dir needs a value")?.into(),
            "--ics" => opts.ics_dir = Some(args.next().ok_or("--ics needs a value")?.into()),
            "--sms" => opts.sms = true,
            "--show-titles" => opts.show_titles = true,
            other if other.starts_with('-') => return Err(format!("unknown option {other}").into()),
            other => account = Some(other.to_owned()),
        }
    }
    opts.account = account.ok_or("usage: reminders_probe <apple-id> [--sms] [--show-titles] [--ics DIR]")?;
    Ok(opts)
}

fn line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim().to_owned())
}

fn sign_in(client: &Client, opts: &Options) -> Result<()> {
    if client.has_saved_session() {
        match client.resume() {
            Ok(()) => {
                println!("Reusing the saved session.");
                return Ok(());
            }
            Err(err) if err.is_auth() => println!("Saved session no longer valid ({err}); signing in again."),
            Err(err) => return Err(err.into()),
        }
    }
    let password = SecretString::from(rpassword::prompt_password(format!("Password for {}: ", opts.account))?);
    if client.login(&password)? == LoginStatus::Authenticated {
        return Ok(());
    }

    let choices = client.two_factor_options();
    let use_sms =
        choices.can_use_sms() && (opts.sms || choices.push_bridge || choices.preferred_method() == CodeMethod::Sms);
    let phone: Option<TrustedPhone> = if use_sms {
        let phone = match choices.phones.as_slice() {
            [] => return Err(Error::NoTrustedPhone.into()),
            [only] => only.clone(),
            many => {
                for (i, p) in many.iter().enumerate() {
                    println!("  {}: {}", i + 1, p.display.as_deref().unwrap_or("(number hidden)"));
                }
                let pick: usize = line("Send the code to number: ")?.parse()?;
                many.get(pick.wrapping_sub(1)).cloned().ok_or("no such number")?
            }
        };
        client.send_sms(&phone)?;
        println!("Code sent by SMS to {}.", phone.display.as_deref().unwrap_or("your trusted number"));
        Some(phone)
    } else {
        println!("Look at your trusted Apple devices for a verification code.");
        None
    };

    // A wrong code counts towards Apple's lockout: stop after three.
    for attempt in 1..=3 {
        let code = line("Verification code: ")?;
        let outcome = match &phone {
            Some(p) => client.verify_sms(p, &code),
            None => client.verify_trusted_device(&code),
        };
        match outcome {
            Ok(()) => return Ok(()),
            Err(Error::WrongCode) if attempt < 3 => println!("Code not accepted."),
            Err(err) => return Err(err.into()),
        }
    }
    Err("verification failed".into())
}

#[derive(Default)]
struct Stats {
    reminders: usize,
    completed: usize,
    with_notes: usize,
    timed_due: usize,
    timed_due_with_millis: usize,
    all_day: usize,
    with_time_zone: usize,
    hashtags: usize,
    recurring: usize,
    subtasks: usize,
    flagged: usize,
    url: usize,
    location_alarms: usize,
    title_decode_failures: usize,
    round_trip_ok: usize,
    round_trip_diffs: BTreeMap<String, usize>,
    lossy: usize,
    /// How all-day due dates are stored: "local HH:MM / UTC HH:MM / tz?".
    all_day_storage: BTreeMap<String, usize>,
    priorities: BTreeMap<i64, usize>,
    first_day_of_week: BTreeMap<u8, usize>,
}

fn fields_that_differ(a: &Reminder, b: &Reminder) -> Vec<&'static str> {
    let mut out = Vec::new();
    let checks: [(&str, bool); 10] = [
        ("title", a.title != b.title),
        ("notes", a.notes != b.notes),
        ("completed", a.completed != b.completed),
        ("completed_date", a.completed_date != b.completed_date),
        ("priority", a.priority != b.priority),
        ("flagged", a.flagged != b.flagged),
        ("time_zone", a.time_zone != b.time_zone),
        ("due_date", a.due_date != b.due_date),
        ("all_day", a.all_day != b.all_day),
        ("parent", a.parent_reminder_id != b.parent_reminder_id),
    ];
    for (name, differs) in checks {
        if differs {
            out.push(name);
        }
    }
    out
}

fn examine(reminder: &Reminder, contents: &ListContents, stats: &mut Stats, opts: &Options) -> Result<()> {
    stats.reminders += 1;
    stats.completed += usize::from(reminder.completed);
    stats.with_notes += usize::from(!reminder.notes.is_empty());
    stats.flagged += usize::from(reminder.flagged);
    stats.with_time_zone += usize::from(reminder.time_zone.is_some());
    stats.subtasks += usize::from(reminder.parent_reminder_id.is_some());
    stats.title_decode_failures += usize::from(reminder.title == "Error Decoding Title");
    *stats.priorities.entry(reminder.priority).or_default() += 1;

    if let Some(due) = reminder.due_date {
        if reminder.all_day {
            stats.all_day += 1;
            let local = reminder
                .time_zone
                .as_deref()
                .and_then(|tz| jiff::tz::TimeZone::get(tz).ok())
                .and_then(|tz| jiff::Timestamp::from_second(due.unix_timestamp()).ok().map(|ts| ts.to_zoned(tz)))
                .map_or_else(|| "--:--".to_owned(), |z| z.strftime("%H:%M").to_string());
            let key = format!(
                "local {local} / UTC {:02}:{:02} / tz {}",
                due.hour(),
                due.minute(),
                if reminder.time_zone.is_some() { "set" } else { "none" }
            );
            *stats.all_day_storage.entry(key).or_default() += 1;
        } else {
            stats.timed_due += 1;
            stats.timed_due_with_millis += usize::from(due.millisecond() != 0);
        }
    }

    let task = Task::from_reminder(reminder, contents);
    stats.hashtags += usize::from(!task.categories.is_empty());
    stats.url += usize::from(task.url.is_some());
    stats.location_alarms += usize::from(!task.locations.is_empty());
    if let Some(rule) = &task.recurrence {
        stats.recurring += 1;
        *stats.first_day_of_week.entry(rule.first_day_of_week).or_default() += 1;
    }

    let ics = task.to_ics();
    if let Some(dir) = &opts.ics_dir {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(format!("{}.ics", task.uid)), &ics)?;
    }

    // Through VTODO text and back: nothing may change.
    let back = Task::from_ics(&ics)?;
    stats.lossy += usize::from(!back.lossy.is_empty());
    let mut copy = reminder.clone();
    let (changed, linked) = back.apply_to(&mut copy, contents);
    let mut diffs: Vec<String> = fields_that_differ(reminder, &copy).into_iter().map(str::to_owned).collect();
    if !linked.hashtags_to_add.is_empty() || !linked.hashtags_to_remove.is_empty() {
        diffs.push("hashtags".into());
    }
    if linked.recurrence != RecurrenceChange::Unchanged {
        diffs.push("recurrence".into());
    }
    if changed && diffs.is_empty() {
        diffs.push("changed-flag".into());
    }
    if diffs.is_empty() && linked == LinkedChanges::default() {
        stats.round_trip_ok += 1;
    } else {
        for d in &diffs {
            *stats.round_trip_diffs.entry(d.clone()).or_default() += 1;
        }
        if opts.show_titles {
            println!("    round trip differs ({}): {:?}", diffs.join(", "), reminder.title);
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let opts = options()?;
    let client = Client::new(ClientConfig::new(opts.account.clone(), Some(opts.session_dir.clone())))?;
    sign_in(&client, &opts)?;

    println!("Opening Reminders (with Advanced Data Protection on, approve the prompt on your Apple device)…");
    let started = Instant::now();
    let service = client.reminders()?;

    let lists = service.lists()?;
    let live: Vec<_> = lists.iter().filter(|l| !l.deleted && !l.is_group).collect();
    println!(
        "\nLists: {} total, {} usable, {} deleted, {} groups",
        lists.len(),
        live.len(),
        lists.iter().filter(|l| l.deleted).count(),
        lists.iter().filter(|l| l.is_group).count()
    );

    let mut stats = Stats::default();
    for (i, list) in live.iter().enumerate() {
        let contents = service.list_contents(&list.id)?;
        let label = if opts.show_titles { format!("{:?}", list.title) } else { format!("list #{}", i + 1) };
        println!(
            "  {label}: {} reminders (list record says {}), {} hashtags, {} rules, {} alarms",
            contents.reminders.len(),
            list.count,
            contents.hashtags.len(),
            contents.recurrence_rules.len(),
            contents.alarms.len()
        );
        for reminder in contents.reminders.iter().filter(|r| !r.deleted) {
            examine(reminder, &contents, &mut stats, &opts)?;
        }
    }

    let cursor = service.sync_cursor()?;
    let (changes, token) = service.changes_since(None)?;
    let updated = changes.iter().filter(|c| matches!(c, ReminderChange::Updated(_))).count();
    let elapsed = started.elapsed();

    println!("\n===== summary (no personal content) =====");
    println!("reminders examined:       {}", stats.reminders);
    println!("  completed:              {}", stats.completed);
    println!("  with notes:             {}", stats.with_notes);
    println!("  timed due date:         {} ({} with milliseconds)", stats.timed_due, stats.timed_due_with_millis);
    println!("  all-day due date:       {}", stats.all_day);
    println!("  with time zone:         {}", stats.with_time_zone);
    println!("  with hashtags:          {}", stats.hashtags);
    println!("  recurring:              {}", stats.recurring);
    println!("  subtasks:               {}", stats.subtasks);
    println!("  flagged:                {}", stats.flagged);
    println!("  with URL:               {}", stats.url);
    println!("  with location alarm:    {}", stats.location_alarms);
    println!("  title decode failures:  {}", stats.title_decode_failures);
    println!("priorities:               {:?}", stats.priorities);
    println!("first day of week values: {:?}", stats.first_day_of_week);
    println!("all-day storage:");
    for (key, n) in &stats.all_day_storage {
        println!("  {key}: {n}");
    }
    println!("VTODO round trip:         {} identical of {}", stats.round_trip_ok, stats.reminders);
    for (field, n) in &stats.round_trip_diffs {
        println!("  differs in {field}: {n}");
    }
    println!("  with RRULE parts lost:  {}", stats.lossy);
    println!(
        "change feed:              {} entries ({updated} updated, {} deleted); cursor {} chars, feed token {} chars",
        changes.len(),
        changes.len() - updated,
        cursor.len(),
        token.len()
    );
    println!("time:                     {:.1}s", elapsed.as_secs_f64());
    if let Some(dir) = &opts.ics_dir {
        println!("VTODO files written to {}", dir.display());
    }
    Ok(())
}
