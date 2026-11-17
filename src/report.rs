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
        let mut s = String::new();
        let size = |n: u64| format_bytes(n, self.units);
        s.push_str(&format!("Number of files scanned:          {:>8}\n", self.files_scanned));
        s.push_str(&format!("Number of files transferred:      {:>8}\n", self.copied_files));
        s.push_str(&format!("Total transferred file size:      {:>8}\n", size(self.copied_bytes)));
        if self.linked_files > 0 {
            s.push_str(&format!("Number of files hard-linked:      {:>8}\n", self.linked_files));
        }
        s.push_str(&format!("Skipped files:                    {:>8}\n", self.skipped_files));
        if self.deleted_files > 0 {
            s.push_str(&format!("Number of files deleted:        {:>8}\n", self.deleted_files));
            s.push_str(&format!("Total deleted file size:        {:>8}\n", size(self.deleted_bytes)));
        }
        if self.error_files > 0 {
            s.push_str(&format!("Errors encountered:             {:>8}\n", self.error_files));
        }
        if self.cancelled_jobs > 0 {
            s.push_str(&format!("Cancelled jobs:                 {:>8}\n", self.cancelled_jobs));
        }
        s.push_str(&format!(
            "Total execution time:             {:>10.2} s\n",
            self.elapsed_secs
        ));
        if self.elapsed_secs > 0.0 {
            let avg = self.copied_bytes as f64 / self.elapsed_secs;
            s.push_str(&format!(
                "Average transfer speed:         {:>12}\n",
                format_speed(avg, self.units)
            ));
        }
        s
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
