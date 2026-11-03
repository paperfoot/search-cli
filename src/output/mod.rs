pub mod json;
pub mod style;
pub mod table;

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

pub enum OutputFormat {
    Json,
    Table,
}

impl OutputFormat {
    pub fn detect(json_flag: bool) -> Self {
        if json_flag || !std::io::stdout().is_terminal() {
            OutputFormat::Json
        } else {
            OutputFormat::Table
        }
    }
}

/// Indented JSON on a terminal or with `--pretty`; one compact line when
/// piped, where an agent pays for every byte.
static PRETTY: AtomicBool = AtomicBool::new(false);

pub fn set_pretty(pretty_flag: bool) {
    PRETTY.store(
        pretty_flag || std::io::stdout().is_terminal(),
        Ordering::Relaxed,
    );
}

pub fn pretty() -> bool {
    PRETTY.load(Ordering::Relaxed)
}

/// `NO_COLOR` set to anything but the empty string turns colour off
/// (no-color.org), as does `TERM=dumb`.
pub fn no_color_env() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        || std::env::var_os("TERM").is_some_and(|v| v == "dumb")
}

/// Colour for stdout: a terminal, and no `NO_COLOR`.
pub fn use_color() -> bool {
    std::io::stdout().is_terminal() && !no_color_env()
}

/// Output context: bundles format + quiet so dispatch sites take one value.
pub struct Ctx {
    pub format: OutputFormat,
    pub quiet: bool,
}

impl Ctx {
    pub fn new(json_flag: bool, quiet: bool) -> Self {
        Self {
            format: OutputFormat::detect(json_flag),
            quiet,
        }
    }

    pub fn is_json(&self) -> bool {
        matches!(self.format, OutputFormat::Json)
    }

    /// True when human output should be suppressed (quiet + table mode).
    pub fn suppress_human(&self) -> bool {
        self.quiet && !self.is_json()
    }
}
