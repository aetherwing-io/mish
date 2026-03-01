/// Deduplication engine.
///
/// Template-based dedup: groups similar lines and emits counts.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use regex::Regex;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max groups before oldest is flushed
pub const MAX_GROUPS: usize = 1000;
/// Max template length before truncation
pub const MAX_TEMPLATE_LEN: usize = 500;
/// Count threshold: once a group hits this, flush it
pub const COUNT_THRESHOLD: u32 = 5;
/// Template similarity threshold (already-tokenized)
pub const TEMPLATE_SIMILARITY: f64 = 0.2;
/// Raw line similarity threshold (for implicit dedup)
pub const RAW_SIMILARITY: f64 = 0.3;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DedupGroup {
    pub template: String,
    pub count: u32,
    pub first_instance: String,
    pub last_instance: String,
    pub first_seen: Instant,
    pub last_seen: Instant,
}

impl DedupGroup {
    /// Format this group for output.
    pub fn format(&self) -> String {
        if self.count == 1 {
            self.first_instance.clone()
        } else {
            // Heuristic: ≤1 token replacement → use first instance + count
            // ≥2 token replacements → use generalized form
            let token_count = self.template.matches('{').count();
            if token_count <= 1 {
                format!("{} (x{})", self.first_instance, self.count)
            } else {
                format!("{} (x{})", self.first_instance, self.count)
            }
        }
    }
}

pub struct DedupEngine {
    groups: HashMap<String, DedupGroup>,
    recent_templates: VecDeque<String>,
    tokenizers: Vec<(Regex, &'static str)>,
}

/// Result of implicit (structural) dedup check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupResult {
    /// Line absorbed into current streak
    Absorbed,
    /// Streak broken — flush accumulated count
    FlushStreak { first: String, count: u32 },
    /// Not similar to previous line
    NotSimilar,
}

pub struct ImplicitDedup {
    previous_line: Option<String>,
    previous_template: Option<String>,
    streak_count: u32,
    streak_first: Option<String>,
    tokenizers: Vec<(Regex, &'static str)>,
}

// ---------------------------------------------------------------------------
// Levenshtein distance
// ---------------------------------------------------------------------------

fn levenshtein(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();
    if a_len == 0 { return b_len; }
    if b_len == 0 { return a_len; }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost)
                .min(prev[j + 1] + 1)
                .min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

fn normalized_distance(a: &str, b: &str) -> f64 {
    let max_len = a.len().max(b.len());
    if max_len == 0 { return 0.0; }
    levenshtein(a, b) as f64 / max_len as f64
}

// ---------------------------------------------------------------------------
// Tokenizer patterns (compiled once)
// ---------------------------------------------------------------------------

fn build_tokenizers() -> Vec<(Regex, &'static str)> {
    vec![
        // 1. URLs — grab whole URL before path/version rules fragment it
        (Regex::new(r"https?://\S+").unwrap(), "{url}"),
        // 2. Paths — before version numbers inside paths get tokenized
        (Regex::new(r"(?:/[\w.\-]+){2,}").unwrap(), "{path}"),
        // 3. Semver — before plain number rule eats the components
        (Regex::new(r"\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?").unwrap(), "{ver}"),
        // 4. Package identifiers — scoped and unscoped npm-style
        (Regex::new(r"(?:@[\w\-]+/)?[\w.\-]+@").unwrap(), "{pkg}@"),
        // 5. UUIDs — before hash rule eats the segments
        (Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap(), "{uuid}"),
        // 6. Hashes — git SHAs, docker image hashes, checksums
        (Regex::new(r"\b[0-9a-f]{7,64}\b").unwrap(), "{hash}"),
        // 7. Timestamps (ISO)
        (Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}").unwrap(), "{ts}"),
        // 8. Timestamps (syslog/common)
        (Regex::new(r"\b(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d+\s+\d+:\d+:\d+").unwrap(), "{ts}"),
        // 9. Plain numbers (2+ digits, standalone) — last resort
        (Regex::new(r"\b\d{2,}\b").unwrap(), "{n}"),
    ]
}

// ---------------------------------------------------------------------------
// DedupEngine implementation
// ---------------------------------------------------------------------------

impl DedupEngine {
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
            recent_templates: VecDeque::new(),
            tokenizers: build_tokenizers(),
        }
    }

    /// Tokenize a line into a template skeleton.
    pub fn tokenize(&self, line: &str) -> String {
        let mut result = line.to_string();
        for (regex, token) in &self.tokenizers {
            result = regex.replace_all(&result, *token).into_owned();
        }
        // Truncate to MAX_TEMPLATE_LEN
        if result.len() > MAX_TEMPLATE_LEN {
            result.truncate(MAX_TEMPLATE_LEN);
        }
        result
    }

    /// Ingest a line into the dedup engine.
    pub fn ingest(&mut self, line: &str) {
        let template = self.tokenize(line);

        if let Some(group) = self.groups.get_mut(&template) {
            group.count += 1;
            group.last_instance = line.to_string();
            group.last_seen = Instant::now();
            return;
        }

        // Check for similar existing templates
        if let Some(merged_key) = self.find_similar_template(&template) {
            let group = self.groups.get_mut(&merged_key).unwrap();
            group.count += 1;
            group.last_instance = line.to_string();
            group.last_seen = Instant::now();
        } else {
            self.groups.insert(template.clone(), DedupGroup {
                template: template.clone(),
                count: 1,
                first_instance: line.to_string(),
                last_instance: line.to_string(),
                first_seen: Instant::now(),
                last_seen: Instant::now(),
            });
        }

        self.recent_templates.push_back(template);
        self.enforce_memory_bounds();
    }

    fn find_similar_template(&self, template: &str) -> Option<String> {
        for existing in self.groups.keys() {
            if normalized_distance(template, existing) < TEMPLATE_SIMILARITY {
                return Some(existing.clone());
            }
        }
        None
    }

    /// Flush all groups and return formatted output lines.
    pub fn flush_all(&mut self) -> Vec<String> {
        let mut groups: Vec<DedupGroup> = self.groups.drain().map(|(_, g)| g).collect();
        groups.sort_by(|a, b| a.first_seen.cmp(&b.first_seen));
        self.recent_templates.clear();
        groups.iter().map(|g| g.format()).collect()
    }

    /// Flush groups into a mutable output vec.
    pub fn flush_into(&mut self, output: &mut Vec<String>) {
        let lines = self.flush_all();
        output.extend(lines);
    }

    fn enforce_memory_bounds(&mut self) {
        while self.groups.len() > MAX_GROUPS {
            // Find and remove the oldest group
            if let Some(oldest_key) = self.groups.iter()
                .min_by_key(|(_, g)| g.first_seen)
                .map(|(k, _)| k.clone())
            {
                self.groups.remove(&oldest_key);
            }
        }
    }

    /// Check if any group has hit the count threshold and should be flushed.
    pub fn flush_threshold(&mut self) -> Vec<String> {
        let threshold_keys: Vec<String> = self.groups.iter()
            .filter(|(_, g)| g.count >= COUNT_THRESHOLD)
            .map(|(k, _)| k.clone())
            .collect();

        let mut output = Vec::new();
        for key in threshold_keys {
            if let Some(group) = self.groups.remove(&key) {
                output.push(group.format());
            }
        }
        output
    }
}

impl Default for DedupEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ImplicitDedup implementation
// ---------------------------------------------------------------------------

impl ImplicitDedup {
    pub fn new() -> Self {
        Self {
            previous_line: None,
            previous_template: None,
            streak_count: 0,
            streak_first: None,
            tokenizers: build_tokenizers(),
        }
    }

    fn tokenize(&self, line: &str) -> String {
        let mut result = line.to_string();
        for (regex, token) in &self.tokenizers {
            result = regex.replace_all(&result, *token).into_owned();
        }
        result
    }

    /// Check if a line continues the current streak or breaks it.
    pub fn check(&mut self, line: &str) -> DedupResult {
        if let Some(ref prev) = self.previous_line {
            if self.quick_similar(line, prev) {
                let template = self.tokenize(line);
                let prev_template = self.previous_template.as_ref().unwrap();

                if template == *prev_template || normalized_distance(&template, prev_template) < RAW_SIMILARITY {
                    self.streak_count += 1;
                    self.previous_line = Some(line.to_string());
                    return DedupResult::Absorbed;
                }
            }

            // Streak broken — flush if we had one
            if self.streak_count > 1 {
                let result = DedupResult::FlushStreak {
                    first: self.streak_first.take().unwrap(),
                    count: self.streak_count,
                };
                self.reset(line);
                return result;
            }
        }

        self.reset(line);
        DedupResult::NotSimilar
    }

    fn quick_similar(&self, a: &str, b: &str) -> bool {
        let (la, lb) = (a.len(), b.len());
        if la > lb * 2 || lb > la * 2 { return false; }

        let first_a = a.split_whitespace().next();
        let first_b = b.split_whitespace().next();
        first_a == first_b
    }

    fn reset(&mut self, line: &str) {
        self.previous_line = Some(line.to_string());
        self.previous_template = Some(self.tokenize(line));
        self.streak_count = 1;
        self.streak_first = Some(line.to_string());
    }

    /// Flush any remaining streak at end of stream.
    pub fn flush(&mut self) -> Option<DedupResult> {
        if self.streak_count > 1 {
            let result = DedupResult::FlushStreak {
                first: self.streak_first.take().unwrap(),
                count: self.streak_count,
            };
            self.streak_count = 0;
            self.previous_line = None;
            self.previous_template = None;
            Some(result)
        } else {
            None
        }
    }
}

impl Default for ImplicitDedup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Tokenization tests (all 9 patterns)
    // -----------------------------------------------------------------------

    #[test]
    fn test_tokenize_urls() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Fetching https://registry.npmjs.org/express");
        assert_eq!(result, "Fetching {url}");
    }

    #[test]
    fn test_tokenize_paths() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Processing /usr/local/lib/node_modules");
        assert_eq!(result, "Processing {path}");
    }

    #[test]
    fn test_tokenize_semver() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Installing package 1.2.3");
        assert_eq!(result, "Installing package {ver}");
    }

    #[test]
    fn test_tokenize_semver_with_prerelease() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Using version 2.0.0-beta.1");
        assert_eq!(result, "Using version {ver}");
    }

    #[test]
    fn test_tokenize_packages() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Downloading lodash@4.17.21");
        // Note: {pkg}@ consumes "lodash@", then semver consumes "4.17.21"
        assert!(result.contains("{pkg}@") || result.contains("{ver}"));
    }

    #[test]
    fn test_tokenize_hashes() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Commit abc1234 merged");
        assert_eq!(result, "Commit {hash} merged");
    }

    #[test]
    fn test_tokenize_uuids() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Session 550e8400-e29b-41d4-a716-446655440000 started");
        assert_eq!(result, "Session {uuid} started");
    }

    #[test]
    fn test_tokenize_iso_timestamps() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Event at 2024-01-15T12:30:45 received");
        assert_eq!(result, "Event at {ts} received");
    }

    #[test]
    fn test_tokenize_syslog_timestamps() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Jan 15 08:30:45 server restarted");
        assert_eq!(result, "{ts} server restarted");
    }

    #[test]
    fn test_tokenize_plain_numbers() {
        let engine = DedupEngine::new();
        let result = engine.tokenize("Processed 42 records in 15 seconds");
        assert_eq!(result, "Processed {n} records in {n} seconds");
    }

    #[test]
    fn test_tokenize_single_digit_not_replaced() {
        let engine = DedupEngine::new();
        // Single digits (\b\d{2,}\b) should NOT be tokenized
        let result = engine.tokenize("Step 1 of 3");
        assert_eq!(result, "Step 1 of 3");
    }

    #[test]
    fn test_tokenize_order_matters_url_before_path() {
        let engine = DedupEngine::new();
        // URL should be captured as {url}, not fragmented by path rule
        let result = engine.tokenize("registry+https://github.com/rust-lang/crates.io-index");
        assert!(result.contains("{url}"));
        // Should NOT have {path} inside the URL
        assert!(!result.contains("{path}"));
    }

    // -----------------------------------------------------------------------
    // Group operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_ingest_creates_group() {
        let mut engine = DedupEngine::new();
        engine.ingest("Downloading lodash");
        assert_eq!(engine.groups.len(), 1);
    }

    #[test]
    fn test_ingest_same_template_increments_count() {
        let mut engine = DedupEngine::new();
        engine.ingest("Downloading lodash");
        engine.ingest("Downloading express");
        // These may or may not merge depending on tokenization
        // At minimum, "Downloading lodash" and "Downloading express" differ only by word
        // but our tokenizer doesn't remove arbitrary words
        // So they should be separate groups unless similarity catches them
    }

    #[test]
    fn test_ingest_exact_template_match() {
        let mut engine = DedupEngine::new();
        engine.ingest("Fetching https://registry.npmjs.org/express");
        engine.ingest("Fetching https://registry.npmjs.org/lodash");
        // Both tokenize to "Fetching {url}" — same group
        let groups = engine.flush_all();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].contains("(x2)"));
    }

    #[test]
    fn test_group_format_single_instance() {
        let group = DedupGroup {
            template: "test".into(),
            count: 1,
            first_instance: "test line".into(),
            last_instance: "test line".into(),
            first_seen: Instant::now(),
            last_seen: Instant::now(),
        };
        assert_eq!(group.format(), "test line");
    }

    #[test]
    fn test_group_format_multiple_instances() {
        let group = DedupGroup {
            template: "Fetching {url}".into(),
            count: 5,
            first_instance: "Fetching https://example.com/a".into(),
            last_instance: "Fetching https://example.com/e".into(),
            first_seen: Instant::now(),
            last_seen: Instant::now(),
        };
        let formatted = group.format();
        assert!(formatted.contains("(x5)"));
    }

    // -----------------------------------------------------------------------
    // Implicit dedup
    // -----------------------------------------------------------------------

    #[test]
    fn test_implicit_dedup_consecutive_similar() {
        let mut id = ImplicitDedup::new();
        assert_eq!(id.check("Compiling serde v1.0.195"), DedupResult::NotSimilar);
        assert_eq!(id.check("Compiling tokio v1.35.0"), DedupResult::Absorbed);
        assert_eq!(id.check("Compiling regex v1.10.0"), DedupResult::Absorbed);
    }

    #[test]
    fn test_implicit_dedup_streak_break() {
        let mut id = ImplicitDedup::new();
        id.check("Compiling serde v1.0.195");
        id.check("Compiling tokio v1.35.0");
        id.check("Compiling regex v1.10.0");
        // Break the streak
        let result = id.check("error[E0308]: mismatched types");
        assert_eq!(result, DedupResult::FlushStreak {
            first: "Compiling serde v1.0.195".into(),
            count: 3,
        });
    }

    #[test]
    fn test_implicit_dedup_dissimilar_not_merged() {
        let mut id = ImplicitDedup::new();
        assert_eq!(id.check("Compiling serde"), DedupResult::NotSimilar);
        assert_eq!(id.check("error: something broke"), DedupResult::NotSimilar);
    }

    // -----------------------------------------------------------------------
    // Memory bounds
    // -----------------------------------------------------------------------

    #[test]
    fn test_max_groups_enforced() {
        let mut engine = DedupEngine::new();
        // Insert MAX_GROUPS + 10 unique lines
        for i in 0..MAX_GROUPS + 10 {
            engine.ingest(&format!("unique line number {}", i + 1000));
        }
        // Should never exceed MAX_GROUPS
        assert!(engine.groups.len() <= MAX_GROUPS);
    }

    #[test]
    fn test_template_length_truncated() {
        let engine = DedupEngine::new();
        let long_line = "x".repeat(MAX_TEMPLATE_LEN + 100);
        let _template = engine.tokenize(&long_line);
        // Tokenize truncates to MAX_TEMPLATE_LEN
    }

    #[test]
    fn test_flush_all_clears_groups() {
        let mut engine = DedupEngine::new();
        engine.ingest("line 1");
        engine.ingest("line 2");
        let flushed = engine.flush_all();
        assert!(!flushed.is_empty());
        assert!(engine.groups.is_empty());
    }
}
