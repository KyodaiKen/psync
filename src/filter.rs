//! Filter rules with rsync-compatible semantics.
//!
//! Rules are evaluated in order, **first match wins** (rsync behaviour):
//! a later `--include` can re-include a path excluded by an earlier
//! `--exclude`, exactly like rsync.
//!
//! Pattern semantics (rsync man page, local subset):
//! * `*`      — matches any string of characters except `/`
//! * `?`      — matches any single character except `/`
//! * `**`     — matches any string of characters, including `/`
//! * A pattern with **no** `/` matches the *name* of any file/dir at any
//!   depth (e.g. `*.tmp`, `cache`).
//! * A pattern **starting** with `/` is anchored to the transfer root
//!   (e.g. `/data/old`).
//! * A pattern containing a `/` (not leading) matches the path, or any
//!   trailing sub-path (e.g. `build/output` matches `a/build/output`).
//! * A trailing `/` restricts the match to directories (e.g. `logs/`).
//!
//! Destination protection: an entry matching any *exclude* rule is also
//! protected from `--delete` (rsync behaviour).

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    /// Keep the entry (`+` / `!`).
    Include,
    /// Drop the entry (`-`, the default).
    Exclude,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub action: RuleAction,
    pub pattern: String,
    pub dir_only: bool,
}

#[derive(Debug, Default, Clone)]
pub struct FilterSet {
    rules: Vec<Rule>,
}

impl FilterSet {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Add a rule from a `-f`/`--filter` spec: `ACTION PATTERN` (action
    /// `-`, `+` or `!`; a bare pattern means exclude).
    pub fn add_spec(&mut self, spec: &str) -> Result<()> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(AppError::Usage("empty filter spec".into()));
        }
        let (action, pattern) = if let Some(rest) = spec.strip_prefix('-') {
            (RuleAction::Exclude, rest)
        } else if let Some(rest) = spec.strip_prefix('+') {
            (RuleAction::Include, rest)
        } else if let Some(rest) = spec.strip_prefix('!') {
            (RuleAction::Include, rest)
        } else {
            (RuleAction::Exclude, spec)
        };
        self.add(action, pattern)
    }

    /// Add a bare include/exclude rule.
    pub fn add(&mut self, action: RuleAction, pattern: &str) -> Result<()> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err(AppError::Usage("empty filter pattern".into()));
        }
        let dir_only = pattern.ends_with('/');
        let stripped = pattern.trim_end_matches('/');
        if stripped.is_empty() {
            return Err(AppError::Usage(
                "filter pattern '/' matches nothing useful; use a name".into(),
            ));
        }
        self.rules.push(Rule {
            action,
            pattern: stripped.to_string(),
            dir_only,
        });
        Ok(())
    }

    /// Load rules from a filter file (one spec per line, `#` comments and
    /// blank lines ignored) — `--exclude-from`/`--include-from`/`--filter-from`.
    pub fn load_file(&mut self, path: &std::path::Path, default_action: RuleAction) -> Result<()> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            AppError::io(
                format!("reading filter file '{}'", path.display()),
                e,
            )
        })?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('-') || line.starts_with('+') || line.starts_with('!') {
                self.add_spec(line)?;
            } else {
                self.add(default_action, line)?;
            }
        }
        Ok(())
    }

    /// Whether `rel` (relative to the transfer root, `/` separated) must be
    /// excluded. rsync include/exclude semantics: an include rule always
    /// takes precedence over exclude rules, regardless of rule order.
    pub fn is_excluded(&self, rel: &str, is_dir: bool) -> bool {
        let mut excluded = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            if pattern_matches(&rule.pattern, rel) {
                match rule.action {
                    RuleAction::Include => return false,
                    RuleAction::Exclude => excluded = true,
                }
            }
        }
        excluded
    }

    /// Whether `rel` is protected from `--delete` by any filter rule
    /// (rsync: files matching include or exclude rules are never deleted
    /// as "extra" destination files).
    pub fn is_protected(&self, rel: &str) -> bool {
        self.rules.iter().any(|rule| pattern_matches(&rule.pattern, rel))
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }
}

/// Match `pattern` against the relative path `rel` (rsync semantics).
fn pattern_matches(pattern: &str, rel: &str) -> bool {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return false;
    }
    let anchored = pattern.starts_with('/');
    let pat = pattern.trim_start_matches('/');
    if !pat.contains('/') {
        // Name-only pattern: match the final path component.
        let base = rel.rsplit('/').next().unwrap_or(rel);
        return glob_match(pat, base);
    }
    if anchored {
        return glob_match(pat, rel);
    }
    // Unanchored pattern containing '/': match the full path or any
    // trailing sub-path (so `build/output` also matches `x/build/output`).
    let mut start = 0usize;
    loop {
        if glob_match(pat, &rel[start..]) {
            return true;
        }
        match rel[start..].find('/') {
            Some(pos) => start += pos + 1,
            None => return false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Lit(char),
    Q,
    /// `*` — any run of non-`/` characters.
    Star,
    /// `**` — any run of characters (may cross `/`).
    DStar,
}

fn tokenize(pattern: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    tokens.push(Token::DStar);
                } else {
                    tokens.push(Token::Star);
                }
            }
            '?' => tokens.push(Token::Q),
            _ => tokens.push(Token::Lit(c)),
        }
    }
    tokens
}

/// Glob matcher supporting `*` (no `/`), `?` (no `/`), `**` (any, may
/// cross `/` — rsync behaviour: `**.log` matches `deep/dir/file.log`).
///
/// Reachability DP over pattern tokens x string positions.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // `a/**/b` must also match `a/b`: `/**/` means "/zero or more
    // components/", which collapses to a single `/` when `**` is empty.
    if pattern.contains("/**/") && glob_match(&pattern.replace("/**/", "/"), text) {
        return true;
    }
    let tokens = tokenize(pattern);
    let s: Vec<char> = text.chars().collect();
    let m = s.len();

    let mut reach = vec![false; m + 1];
    reach[0] = true;
    for token in &tokens {
        let mut next = vec![false; m + 1];
        match token {
            Token::Lit(c) => {
                for j in 0..m {
                    if reach[j] && s[j] == *c {
                        next[j + 1] = true;
                    }
                }
            }
            Token::Q => {
                for j in 0..m {
                    if reach[j] && s[j] != '/' {
                        next[j + 1] = true;
                    }
                }
            }
            Token::Star => {
                let mut run_ok = false;
                for j in 0..=m {
                    if reach[j] {
                        run_ok = true;
                    }
                    if run_ok {
                        next[j] = true;
                    }
                    if j < m && s[j] == '/' {
                        run_ok = false;
                    }
                }
            }
            Token::DStar => {
                let mut any = false;
                for j in 0..=m {
                    if reach[j] {
                        any = true;
                    }
                    if any {
                        next[j] = true;
                    }
                }
            }
        }
        reach = next;
    }
    reach[m]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(pattern: &str, rel: &str, is_dir: bool) -> bool {
        let mut f = FilterSet::new();
        f.add(RuleAction::Exclude, pattern).unwrap();
        f.is_excluded(rel, is_dir)
    }

    #[test]
    fn name_only_patterns() {
        assert!(ex("*.tmp", "a/b/c.tmp", false));
        assert!(ex("*.tmp", "c.tmp", false));
        assert!(!ex("*.tmp", "a/b/c.txt", false));
        assert!(ex("cache", "x/y/cache", true));
        assert!(!ex("cache", "x/y/cache/file.txt", false));
        assert!(ex("?", "a", false));
        assert!(!ex("?", "ab", false));
    }

    #[test]
    fn slash_patterns() {
        assert!(ex("build/output", "x/build/output", true));
        assert!(ex("build/output", "build/output", true));
        assert!(!ex("build/output", "build/other", true));
        assert!(ex("/data/old", "data/old", true));
        assert!(!ex("/data/old", "x/data/old", true));
    }

    #[test]
    fn double_star() {
        assert!(ex("**/node_modules", "a/node_modules", true));
        assert!(ex("a/**/b", "a/x/y/b", true));
        assert!(ex("a/**/b", "a/b", true));
        assert!(ex("**.log", "deep/dir/file.log", false));
    }

    #[test]
    fn first_match_wins() {
        let mut f = FilterSet::new();
        f.add(RuleAction::Exclude, "data/*").unwrap();
        f.add(RuleAction::Include, "data/keep.txt").unwrap();
        assert!(!f.is_excluded("data/keep.txt", false));
        assert!(f.is_excluded("data/drop.txt", false));
    }

    #[test]
    fn dir_only() {
        assert!(ex("logs/", "x/logs", true));
        assert!(!ex("logs/", "x/logs.txt", false));
    }

    #[test]
    fn protection() {
        let mut f = FilterSet::new();
        f.add(RuleAction::Exclude, "*.tmp").unwrap();
        assert!(f.is_protected("a/b.tmp"));
        assert!(!f.is_protected("a/b.txt"));
    }
}
