//! Terminal presentation: colour, sections, status lines, and spinners.
//!
//! All ANSI escapes, tty detection, and `NO_COLOR` handling live here so the
//! rest of the crate emits output by *intent* (a section, the plan, a finished
//! step) rather than by escape code. `push` drives a [`Console`]; `doctor`
//! reuses the [`Palette`] colour switch but keeps its own report layout.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anstyle::{AnsiColor, Color, Style};
use anyhow::Result;

/// A colour switch wrapping anstyle. When disabled (stdout is not a tty, or
/// `NO_COLOR` is set) every style is empty, so `render*()` emits nothing and
/// callers never have to branch on whether colour is on.
pub struct Palette {
    enabled: bool,
}

impl Palette {
    pub fn detect() -> Self {
        Self {
            enabled: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn maybe(&self, s: Style) -> Style {
        if self.enabled { s } else { Style::new() }
    }

    fn ansi(&self, c: AnsiColor) -> Style {
        self.maybe(Style::new().fg_color(Some(Color::Ansi(c))))
    }

    pub fn green(&self) -> Style {
        self.ansi(AnsiColor::Green)
    }
    pub fn yellow(&self) -> Style {
        self.ansi(AnsiColor::Yellow)
    }
    pub fn red(&self) -> Style {
        self.ansi(AnsiColor::Red)
    }
    pub fn bold(&self) -> Style {
        self.maybe(Style::new().bold())
    }
    pub fn dim(&self) -> Style {
        self.maybe(Style::new().dimmed())
    }
}

/// High-level console for the `push` command. Groups output into sections and
/// renders status lines and spinners. Construct once per run with [`detect`].
///
/// [`detect`]: Console::detect
pub struct Console {
    palette: Palette,
    /// Whether to animate spinners. Tied to the palette's colour switch: a
    /// non-tty stdout neither colours nor animates, so piped output stays a
    /// clean sequence of plain status lines.
    animate: bool,
}

impl Console {
    pub fn detect() -> Self {
        let palette = Palette::detect();
        let animate = palette.enabled;
        Console { palette, animate }
    }

    /// Print the program banner once, at the very top.
    pub fn banner(&self) {
        let s = self.palette.bold();
        println!("{}stackymcstackface{}", s.render(), s.render_reset());
    }

    /// Start a new section with a bold heading, preceded by a blank line.
    pub fn section(&self, title: &str) {
        let s = self.palette.bold();
        println!("\n{}{title}{}", s.render(), s.render_reset());
    }

    /// An indented context line, e.g. the merge-target summary.
    pub fn field(&self, text: &str) {
        println!("  {text}");
    }

    /// The plan: a headline sentence plus an optional dim follow-on (the
    /// existing PR number, or the stacked-on parent).
    pub fn plan(&self, headline: &str, note: Option<&str>) {
        println!("  {headline}");
        if let Some(note) = note {
            let d = self.palette.dim();
            println!("    {}{note}{}", d.render(), d.render_reset());
        }
    }

    /// A completed action: a green check glyph followed by `text`.
    pub fn done(&self, text: &str) {
        let g = self.palette.green();
        println!("  {}✔{} {text}", g.render(), g.render_reset());
    }

    /// A dim line: a lead-in before streamed git output, or a secondary note
    /// under a result.
    pub fn dim_line(&self, text: &str) {
        let d = self.palette.dim();
        println!("  {}{text}{}", d.render(), d.render_reset());
    }

    /// A warning. Multi-line messages keep an indent on continuation lines.
    pub fn warn(&self, text: &str) {
        let y = self.palette.yellow();
        let mut lines = text.lines();
        if let Some(first) = lines.next() {
            println!("  {}⚠{} {first}", y.render(), y.render_reset());
        }
        for line in lines {
            println!("    {line}");
        }
    }

    /// The headline result: a URL on its own line, set off by a blank line and
    /// rendered bold so it is easy to spot and copy.
    pub fn url(&self, url: &str) {
        let s = self.palette.bold();
        println!("\n  {}{url}{}", s.render(), s.render_reset());
    }

    /// Run `work` while showing an animated spinner labelled `pending`. The
    /// spinner line is erased when `work` returns (Ok or Err) and the caller
    /// prints any follow-up. With animation disabled, `work` simply runs and
    /// nothing is drawn -- so spinners are invisible in piped output.
    ///
    /// Only wrap work that produces *no* output of its own: the spinner owns
    /// the line it animates, so a closure that prints to stdout would clash
    /// with it.
    pub fn spinner<T>(&self, pending: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.animate {
            return work();
        }
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        // Render the dim style to owned strings up front so the worker thread
        // moves only `String`s, not borrowed/style state.
        let dim = self.palette.dim();
        let pre = dim.render().to_string();
        let post = dim.render_reset().to_string();
        let label = pending.to_string();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut i = 0usize;
            while !stop_worker.load(Ordering::Relaxed) {
                print!(
                    "\r  {pre}{frame}{post} {label}…",
                    frame = FRAMES[i % FRAMES.len()]
                );
                let _ = io::stdout().flush();
                i = i.wrapping_add(1);
                thread::sleep(Duration::from_millis(80));
            }
        });

        let result = work();
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
        print!("\r\x1b[2K"); // carriage return + erase line
        let _ = io::stdout().flush();
        result
    }
}

/// Print an error line to stderr, red when stderr is a tty. Used by `main`'s
/// top-level handler, which has no [`Console`] in scope.
pub fn print_error(msg: &str) {
    if io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        let r = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));
        eprintln!("{}✗{} {msg}", r.render(), r.render_reset());
    } else {
        eprintln!("error: {msg}");
    }
}
