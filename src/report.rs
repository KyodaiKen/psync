//! Final summary and itemized (`--itemize-changes`) output.
//!
//! Itemize line format (documented in the README):
//!
//! ```text
//!   <type><action><flags>  rel/path
//! ```
//!
//! * type:    `f` file, `d` dir, `l` symlink, `-` other
//! * action:  `c` copied, `l` hard-linked, `u` updated, `=` up to date,
//!            `s` skipped, `d` deleted, `!` error
//! * flags:   any of `p t o g L s` (perms, times, owner, group, link,
//!            size) or `a` when all apply; blank otherwise

use std::io::Write;

use crate::fsutil::{format_bytes, format_speed, Units};
use crate::progress;

#[derive(Debug, Clone)]
pub struct ItemizeLine {
    pub kind: char,
    pub action: char,
    pub flags: String,
    pub rel: String,
    pub bytes: u64,
}

impl ItemizeLine {
    /// Render in the custom `out-format` if given; supports the tokens
    /// `%i` (itemize triple), `%n` (name), `%b` (bytes).
    pub fn render(&self, fmt: Option<&str>, _human: bool, units: Units) -> String {
        let triple = format!("{}{}{}", self.kind, self.action, self.flags);
        match fmt {
            Some(f) if !f.is_empty() => f.replace("%i", &triple).replace("%n", &self.rel).replace("%b", &format_bytes(self.bytes, units)),
            _ => format!("{:<10} {}", triple, self.rel),
        }
    }
}

/// `--stats`-style summary block.
pub struct Summary {
    pub files_scanned: u64,
    pub dirs_scanned: u64,
    pub links_scanned: u64,
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub linked_files: u64,
    pub skipped_files: u64,
    pub deleted_files: u64,
    pub deleted_bytes: u64,
    pub error_files: u64,
    pub cancelled_jobs: u64,
    pub elapsed_secs: f64,
    pub human: bool,
    pub units: Units,
}

impl Summary {
    pub fn render(&self) -> String {
        let color = progress::color_enabled();
        let size = |n: u64| format_bytes(n, self.units);
        let mut rows: Vec<(&str, String)> = Vec::new();
        rows.push(("Number of files scanned", self.files_scanned.to_string()));
        rows.push(("Number of files transferred", self.copied_files.to_string()));
        rows.push(("Total transferred file size", size(self.copied_bytes)));
        if self.linked_files > 0 {
            rows.push(("Number of files hard-linked", self.linked_files.to_string()));
        }
        rows.push(("Skipped files", self.skipped_files.to_string()));
        if self.deleted_files > 0 {
            rows.push(("Number of files deleted", self.deleted_files.to_string()));
            rows.push(("Total deleted file size", size(self.deleted_bytes)));
        }
        if self.error_files > 0 {
            rows.push(("Errors encountered", self.error_files.to_string()));
        }
        if self.cancelled_jobs > 0 {
            rows.push(("Cancelled jobs", self.cancelled_jobs.to_string()));
        }
        rows.push(("Total execution time", format!("{:.2} s", self.elapsed_secs)));
        if self.elapsed_secs > 0.0 {
            let avg = self.copied_bytes as f64 / self.elapsed_secs;
            rows.push(("Average transfer speed", format_speed(avg, self.units)));
        }
        let mut s = String::new();
        for (label, value) in rows {
            s.push_str(&row(label, &value, color));
            s.push('\n');
        }
        s
    }
}

/// One summary row: `Label .... : value` — the dot leader keeps the colon
/// (and therefore every value) on a fixed column. Colored to match the
/// live UI; plain when colors are off.
fn row(label: &str, value: &str, color: bool) -> String {
    const COLON_COL: usize = 35; // 0-based column where ':' sits
    let dots = ".".repeat(COLON_COL.saturating_sub(label.len() + 2).max(2));
    if color {
        format!(
            "\x1b[38;5;250m{label}\x1b[0m  \x1b[38;5;236m{dots}\x1b[0m : \x1b[1;38;5;255m{value}\x1b[0m"
        )
    } else {
        format!("{label}  {dots} : {value}")
    }
}

/// Write itemized lines to a log file (`--log-file`).
pub fn write_log_file(path: &std::path::Path, lines: &[ItemizeLine]) -> std::io::Result<()> {
    use std::io::BufWriter;
    let f = std::fs::File::create(path)?;
    let mut w = BufWriter::new(f);
    for line in lines {
        writeln!(w, "{}", line.render(None, false, Units::Jedec))?;
    }
    Ok(())
}

/// Print error lines (always, regardless of verbosity).
pub fn print_errors(errors: &[crate::engine_ctx::JobError]) {
    for e in errors {
        eprintln!("error: [{}] {}", e.rel, crate::error::format_chain(&e.error));
    }
}
