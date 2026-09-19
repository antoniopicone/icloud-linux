//! Talking to the person at the terminal.

use std::io::{self, BufRead, Write};

/// What the interactive commands need from a terminal, so tests can script it.
pub trait Prompter {
    /// Show `text` and read a line.
    fn line(&mut self, text: &str) -> io::Result<String>;
    /// Show `text` and read a line without echoing it.
    fn secret(&mut self, text: &str) -> io::Result<String>;
    fn say(&mut self, text: &str);
}

/// The real terminal.
#[derive(Debug, Default)]
pub struct Terminal;

impl Prompter for Terminal {
    fn line(&mut self, text: &str) -> io::Result<String> {
        print!("{text}");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no input available"));
        }
        Ok(line.trim().to_owned())
    }

    fn secret(&mut self, text: &str) -> io::Result<String> {
        rpassword::prompt_password(text)
    }

    fn say(&mut self, text: &str) {
        println!("{text}");
    }
}

/// Ask a yes/no question; anything but an explicit yes is no.
pub fn confirm(ui: &mut dyn Prompter, question: &str) -> bool {
    ui.line(&format!("{question} [y/N] "))
        .is_ok_and(|answer| matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// Answers prompts from a script and records everything shown.
    #[derive(Debug, Default)]
    pub(crate) struct Script {
        pub(crate) answers: VecDeque<String>,
        pub(crate) said: Vec<String>,
        pub(crate) asked: Vec<String>,
    }

    impl Script {
        pub(crate) fn new(answers: &[&str]) -> Self {
            Self { answers: answers.iter().map(|s| (*s).to_owned()).collect(), ..Self::default() }
        }

        fn next(&mut self, text: &str) -> io::Result<String> {
            self.asked.push(text.to_owned());
            self.answers.pop_front().ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "script ran out"))
        }

        pub(crate) fn said_contains(&self, needle: &str) -> bool {
            self.said.iter().any(|s| s.contains(needle))
        }
    }

    impl Prompter for Script {
        fn line(&mut self, text: &str) -> io::Result<String> {
            self.next(text)
        }
        fn secret(&mut self, text: &str) -> io::Result<String> {
            self.next(text)
        }
        fn say(&mut self, text: &str) {
            self.said.push(text.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::Script, *};

    #[test]
    fn only_an_explicit_yes_confirms() {
        for (answer, expected) in [("y", true), ("YES", true), ("n", false), ("", false), ("maybe", false)] {
            assert_eq!(confirm(&mut Script::new(&[answer]), "sure?"), expected, "{answer:?}");
        }
        assert!(!confirm(&mut Script::new(&[]), "sure?"), "no input means no");
    }
}
