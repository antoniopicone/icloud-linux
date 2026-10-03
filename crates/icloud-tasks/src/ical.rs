//! Just enough iCalendar (RFC 5545) to read and write `VTODO`s: content lines,
//! parameters, components, TEXT escaping and line folding.
//!
//! Reading is lenient (LF or CRLF, unknown properties kept as they are);
//! writing is strict (CRLF, lines folded at 75 octets without splitting a
//! UTF-8 sequence).

use std::fmt::Write as _;

use crate::Error;

/// One content line, e.g. `DUE;VALUE=DATE:20260923`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// Upper-cased name.
    pub name: String,
    /// Parameters in order, names upper-cased, values unquoted.
    pub params: Vec<(String, String)>,
    /// The raw value, still escaped where the value type escapes.
    pub value: String,
}

impl Property {
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Self { name: name.to_ascii_uppercase(), params: Vec::new(), value: value.into() }
    }

    /// A TEXT property: `value` is escaped here.
    pub fn text(name: &str, value: &str) -> Self {
        Self::new(name, escape_text(value))
    }

    #[must_use]
    pub fn with_param(mut self, name: &str, value: &str) -> Self {
        self.params.push((name.to_ascii_uppercase(), value.to_owned()));
        self
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The value as TEXT, unescaped.
    pub fn text_value(&self) -> String {
        unescape_text(&self.value)
    }

    /// A list value (CATEGORIES): split on unescaped commas, each unescaped.
    pub fn text_list(&self) -> Vec<String> {
        split_unescaped(&self.value, ',').iter().map(|item| unescape_text(item)).filter(|s| !s.is_empty()).collect()
    }
}

/// A component (`VCALENDAR`, `VTODO`, `VALARM`, …) and what it contains.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Component {
    pub name: String,
    pub properties: Vec<Property>,
    pub components: Vec<Component>,
}

impl Component {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_ascii_uppercase(), ..Self::default() }
    }

    pub fn property(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn properties_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.properties.iter().filter(move |p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn push(&mut self, property: Property) {
        self.properties.push(property);
    }

    /// The first sub-component with this name.
    pub fn component(&self, name: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Serialise with CRLF line ends and folded lines.
    pub fn to_ics(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        write_line(out, &format!("BEGIN:{}", self.name));
        for property in &self.properties {
            let mut line = property.name.clone();
            for (name, value) in &property.params {
                let _ = write!(line, ";{name}={}", quote_param(value));
            }
            line.push(':');
            line.push_str(&property.value);
            write_line(out, &line);
        }
        for child in &self.components {
            child.write(out);
        }
        write_line(out, &format!("END:{}", self.name));
    }
}

/// Parse an iCalendar object. Returns the outermost component.
pub fn parse(input: &str) -> Result<Component, Error> {
    let mut stack: Vec<Component> = Vec::new();
    let mut root: Option<Component> = None;

    for line in unfold(input) {
        if line.trim().is_empty() {
            continue;
        }
        let property = parse_line(&line)?;
        match property.name.as_str() {
            "BEGIN" => stack.push(Component::new(property.value.trim())),
            "END" => {
                let done = stack.pop().ok_or_else(|| Error::Ical(format!("END:{} without BEGIN", property.value)))?;
                if !done.name.eq_ignore_ascii_case(property.value.trim()) {
                    return Err(Error::Ical(format!("END:{} closes {}", property.value, done.name)));
                }
                match stack.last_mut() {
                    Some(parent) => parent.components.push(done),
                    None if root.is_none() => root = Some(done),
                    None => return Err(Error::Ical("more than one top-level component".into())),
                }
            }
            _ => stack
                .last_mut()
                .ok_or_else(|| Error::Ical(format!("property {} outside any component", property.name)))?
                .properties
                .push(property),
        }
    }
    if let Some(open) = stack.last() {
        return Err(Error::Ical(format!("{} is never closed", open.name)));
    }
    root.ok_or_else(|| Error::Ical("no component found".into()))
}

/// Join folded lines: a line starting with a space or tab continues the
/// previous one, minus that one character.
fn unfold(input: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in input.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(rest);
        } else {
            lines.push(raw.to_owned());
        }
    }
    lines
}

fn parse_line(line: &str) -> Result<Property, Error> {
    // name, then ;param=value (values may be "quoted"), then :value.
    let mut chars = line.char_indices().peekable();
    let mut name_end = None;
    for (i, c) in chars.by_ref() {
        if c == ';' || c == ':' {
            name_end = Some((i, c));
            break;
        }
    }
    let (name_end, mut sep) = name_end.ok_or_else(|| Error::Ical(format!("not a content line: {line:?}")))?;
    let name = line[..name_end].trim().to_ascii_uppercase();
    if name.is_empty() {
        return Err(Error::Ical(format!("content line without a name: {line:?}")));
    }

    let mut params = Vec::new();
    while sep == ';' {
        let mut pname = String::new();
        let mut pvalue = String::new();
        let mut in_value = false;
        let mut quoted = false;
        let mut next_sep = None;
        for (_, c) in chars.by_ref() {
            match c {
                '"' if in_value => quoted = !quoted,
                '=' if !in_value => in_value = true,
                ';' | ':' if !quoted => {
                    next_sep = Some(c);
                    break;
                }
                _ if in_value => pvalue.push(c),
                _ => pname.push(c),
            }
        }
        params.push((pname.trim().to_ascii_uppercase(), pvalue));
        sep = next_sep.ok_or_else(|| Error::Ical(format!("content line without a value: {line:?}")))?;
    }
    let value: String = chars.map(|(_, c)| c).collect();
    Ok(Property { name, params, value })
}

fn quote_param(value: &str) -> String {
    if value.contains([':', ';', ',']) { format!("\"{}\"", value.replace('"', "'")) } else { value.to_owned() }
}

/// Append `line` folded at 75 octets, then CRLF.
fn write_line(out: &mut String, line: &str) {
    let mut width = 0;
    for c in line.chars() {
        let len = c.len_utf8();
        if width + len > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += len;
    }
    out.push_str("\r\n");
}

pub fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

pub fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn split_unescaped(value: &str, sep: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            if let Some(last) = parts.last_mut() {
                last.push('\\');
                last.push(c);
            }
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == sep {
            parts.push(String::new());
        } else if let Some(last) = parts.last_mut() {
            last.push(c);
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_lines_and_quoted_params_are_read() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VTODO\nSUMMARY:Comprare il la\n tte\nDUE;TZID=\"Europe/Rome\";X-A=\"a:b;c\":20260923T100000\nEND:VTODO\nEND:VCALENDAR\n";
        let cal = parse(ics).unwrap();
        let todo = cal.component("VTODO").unwrap();
        assert_eq!(todo.property("summary").unwrap().text_value(), "Comprare il latte");
        let due = todo.property("DUE").unwrap();
        assert_eq!(due.param("tzid"), Some("Europe/Rome"));
        assert_eq!(due.param("X-A"), Some("a:b;c"));
        assert_eq!(due.value, "20260923T100000");
    }

    #[test]
    fn long_lines_fold_at_75_octets_without_splitting_utf8() {
        let mut c = Component::new("VTODO");
        c.push(Property::text("SUMMARY", &"è".repeat(100)));
        let ics = c.to_ics();
        for line in ics.split("\r\n") {
            assert!(line.len() <= 75, "{} octets: {line}", line.len());
        }
        let back = parse(&ics).unwrap();
        assert_eq!(back.property("SUMMARY").unwrap().text_value(), "è".repeat(100));
    }

    #[test]
    fn text_escaping_round_trips() {
        let text = "a, b; c\\d\nsecond line";
        assert_eq!(unescape_text(&escape_text(text)), text);
        assert_eq!(escape_text(text), "a\\, b\\; c\\\\d\\nsecond line");
    }

    #[test]
    fn category_lists_split_on_unescaped_commas_only() {
        let p = Property::new("CATEGORIES", "casa,lavoro\\, ufficio,,spesa");
        assert_eq!(p.text_list(), vec!["casa", "lavoro, ufficio", "spesa"]);
    }

    #[test]
    fn malformed_input_is_an_error() {
        assert!(parse("BEGIN:VTODO\nSUMMARY:x\n").is_err());
        assert!(parse("BEGIN:VTODO\nEND:VEVENT\n").is_err());
        assert!(parse("SUMMARY:x\n").is_err());
        assert!(parse("").is_err());
    }
}
