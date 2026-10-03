//! iCloud Reminders as iCalendar `VTODO`s.
//!
//! This crate is the translation layer between the Reminders service of
//! `icloud-api` and a `CalDAV` task list (a local Radicale, read by Planify,
//! GNOME and Evolution):
//!
//! * [`ical`]: a small, strict-on-write iCalendar reader and writer;
//! * [`vtodo`]: [`Task`], the shape a reminder and a VTODO share, with
//!   conversions both ways and the list of linked records (hashtags,
//!   recurrence rule) a `CalDAV` edit implies.
//!
//! The sync daemon that moves tasks between the two sides builds on this.

pub mod ical;
pub mod vtodo;

pub use vtodo::{Due, LinkedChanges, Recurrence, RecurrenceChange, Task};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid iCalendar data: {0}")]
    Ical(String),
    #[error("the calendar object holds no VTODO")]
    NoTodo,
    #[error("the VTODO has no UID")]
    MissingUid,
    #[error("unreadable date or date-time: {0}")]
    BadDate(String),
}
