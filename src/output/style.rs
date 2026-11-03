//! Terminal colour that honours `NO_COLOR`. The methods mirror the subset of
//! `owo_colors::OwoColorize` the crate used, so call sites read the same, but
//! each returns plain text when [`super::use_color`] is false.

use std::fmt::Display;
use std::sync::OnceLock;

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(super::use_color)
}

fn paint(text: &(impl Display + ?Sized), code: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub trait Paint: Display {
    fn bold(&self) -> String {
        paint(self, "1")
    }
    fn dimmed(&self) -> String {
        paint(self, "2")
    }
    fn underline(&self) -> String {
        paint(self, "4")
    }
    fn black(&self) -> String {
        paint(self, "30")
    }
    fn red(&self) -> String {
        paint(self, "31")
    }
    fn green(&self) -> String {
        paint(self, "32")
    }
    fn yellow(&self) -> String {
        paint(self, "33")
    }
    fn blue(&self) -> String {
        paint(self, "34")
    }
    fn cyan(&self) -> String {
        paint(self, "36")
    }
    fn white(&self) -> String {
        paint(self, "37")
    }
    fn on_green(&self) -> String {
        paint(self, "42")
    }
    fn on_cyan(&self) -> String {
        paint(self, "46")
    }
}

impl<T: Display + ?Sized> Paint for T {}
