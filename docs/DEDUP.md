# Deduplication Engine

## Overview

The dedup engine collapses repetitive terminal output into single lines with counts. It operates on lines classified as `Noise(Dedup)` by grammar rules or structural heuristics, plus unmatched lines that pass the edit-distance similarity check.

```
Input:
  Downloading lodash@4.17.21
  Downloading express@4.18.2
  Downloading react@18.2.0
  Downloading react-dom@18.2.0
  ... 143 more

Output:
  Downloading packages (x147)
```

## Tokenization Pipeline

The core of dedup is converting concrete lines into abstract "template skeletons" that can be compared and grouped.

### Token types and patterns

Applied in this order. Order matters — earlier rules are more specific and prevent over-tokenization by later rules.

```
1. URLs
   Pattern:  https?://\S+
   Token:    {url}
   Reason:   Grab whole URL before path/version rules fragment it

2. Paths
   Pattern:  (?:/[\w.-]+){2,}
   Token:    {path}
   Reason:   Before version numbers inside paths get tokenized

3. Semver
   Pattern:  \d+\.\d+\.\d+(-[\w.]+)?(\+[\w.]+)?
   Token:    {ver}
   Reason:   Before plain number rule eats the components

4. Package identifiers
   Pattern:  (@[\w-]+/)?[\w.-]+@       (name@ prefix before version)
   Token:    {pkg}@
   Reason:   Scoped and unscoped npm-style packages

5. Hashes
   Pattern:  \b[0-9a-f]{7,64}\b
   Token:    {hash}
   Reason:   Git SHAs, Docker image hashes, checksums

6. UUIDs
   Pattern:  [0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}
   Token:    {uuid}
   Reason:   Before hash rule eats the segments

7. Timestamps (ISO)
   Pattern:  \d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}
   Token:    {ts}
   Reason:   Before plain numbers eat date components

8. Timestamps (syslog/common)
   Pattern:  \b(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d+\s+\d+:\d+:\d+
   Token:    {ts}

9. Plain numbers
   Pattern:  \b\d{2,}\b
   Token:    {n}
   Reason:   Last resort — most aggressive, only standalone 2+ digit numbers
```

### Tokenization example

```
Input:   "Compiling serde v1.0.195 (registry+https://github.com/rust-lang/crates.io-index)"
Step 1:  URLs  → "Compiling serde v1.0.195 (registry+{url})"
Step 2:  Paths → (no change, URL already captured)
Step 3:  Semver → "Compiling serde v{ver} (registry+{url})"
Step 4:  Pkgs  → (no change)
Step 5-9: (no change)

Template: "Compiling serde v{ver} (registry+{url})"
```

Wait — "serde" is also variable across lines. But we don't tokenize arbitrary words (too aggressive). The edit-distance similarity check handles this at the grouping stage.

### Improved tokenization: contextual word tokens

After applying the fixed token patterns, check if the resulting templates cluster with high similarity. If multiple templates differ by only one or two words, merge them:

```
"Compiling serde v{ver} (registry+{url})"
"Compiling tokio v{ver} (registry+{url})"
"Compiling regex v{ver} (registry+{url})"

These differ only in one word position.
Merged template: "Compiling {word} v{ver} (registry+{url})"
```

This is done in the grouping phase, not tokenization. Tokenization is deterministic and fast. Merging is a second pass over accumulated groups.

## Grouping

### Data structure

```rust
struct DedupGroup {
    template: String,              // skeleton key
    count: u32,                    // lines in this group
    first_instance: String,        // original first line (for display)
    last_instance: String,         // most recent line
    first_seen: Instant,
    last_seen: Instant,
}

struct DedupEngine {
    groups: HashMap<String, DedupGroup>,
    recent_templates: VecDeque<String>,  // for merge detection
}
```

### Ingestion

```rust
impl DedupEngine {
    fn ingest(&mut self, line: &str) {
        let template = self.tokenize(line);

        match self.groups.get_mut(&template) {
            Some(group) => {
                group.count += 1;
                group.last_instance = line.to_string();
                group.last_seen = Instant::now();
            }
            None => {
                // Check if this template is similar to an existing one
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
            }
        }
    }

    fn find_similar_template(&self, template: &str) -> Option<String> {
        for existing in self.groups.keys() {
            let max_len = template.len().max(existing.len());
            if max_len == 0 { continue; }
            let distance = levenshtein(template, existing);
            let normalized = distance as f64 / max_len as f64;
            if normalized < 0.2 {  // templates are very similar
                return Some(existing.clone());
            }
        }
        None
    }
}
```

### Template similarity threshold

Template-to-template comparison uses a **tighter** threshold (0.2) than raw line comparison (0.3) because templates have already had variable parts removed. If two templates are still 80%+ identical, they're almost certainly the same pattern with a remaining un-tokenized variable.

## Flush Strategy

Dedup groups don't emit immediately. They accumulate and flush on specific triggers.

### Flush triggers

```
1. Process exit         → flush all groups, include in final summary
2. Silence (>2s)        → flush accumulated groups
3. Severity change      → a hazard line arrives; flush groups first
                          to preserve chronological ordering
4. Count threshold      → group hits N instances (default: 5);
                          emit once and stop tracking to avoid
                          unbounded memory growth
5. Periodic timer (5s)  → flush for long-running processes
```

### Flush output format

```rust
impl DedupGroup {
    fn format(&self) -> String {
        if self.count == 1 {
            // Single instance — show the original line
            self.first_instance.clone()
        } else {
            // Multiple instances — show template with count
            let display = self.simplify_template();
            format!("{} (x{})", display, self.count)
        }
    }

    fn simplify_template(&self) -> String {
        // Convert template skeleton to human-friendly form
        // "Downloading {pkg}@{ver} from {url}" → "Downloading packages"
        // "Compiling {word} v{ver} ({url})" → "Compiling crates"

        // Strategy: take the first concrete word(s) and pluralize/generalize
        // Fall back to the first_instance if template is too abstract
        // ...
    }
}
```

### Examples

```
Group: template="Downloading {pkg}@{ver}" count=147
Output: "Downloading packages (x147)"

Group: template="npm warn deprecated {pkg}@{ver}" count=3
Output: "npm warn deprecated: inflight@1.0.6 (x3)"
  (uses first_instance for the specific example, appends count)

Group: template="Compiling {word} v{ver}" count=89
Output: "Compiling crates (x89)"

Group: template="Processing file: {path}" count=47
Output: "Processing files (x47)"

Group: count=1
Output: "Downloading lodash@4.17.21"
  (original line, no count)
```

### Display preference

When count > 1, choose between two display modes:

1. **First instance + count**: `"npm warn deprecated inflight@1.0.6 (x3)"`
   - When the first instance is informative and representative
   - When the user might want to know a specific example

2. **Generalized template + count**: `"Downloading packages (x147)"`
   - When the instances are interchangeable
   - When showing a specific one would be misleading (suggests only that one)

Heuristic: if the template has ≤1 token replacement, use mode 1. If it has ≥2, use mode 2.

## Implicit Dedup (Structural)

Lines not tagged `Dedup` by grammar rules can still be deduped through the structural heuristics tier. This handles unknown tools.

### Consecutive similarity detection

```rust
struct ImplicitDedup {
    previous_line: Option<String>,
    previous_template: Option<String>,
    streak_count: u32,
    streak_first: Option<String>,
}

impl ImplicitDedup {
    fn check(&mut self, line: &str) -> DedupResult {
        if let Some(ref prev) = self.previous_line {
            // Quick pre-filter: similar length and same first token
            if self.quick_similar(line, prev) {
                let template = tokenize(line);
                let prev_template = self.previous_template.as_ref().unwrap();

                if template == *prev_template || edit_distance_similar(&template, prev_template) {
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
        // Length within 2x
        let (la, lb) = (a.len(), b.len());
        if la > lb * 2 || lb > la * 2 { return false; }

        // Same first token
        let first_a = a.split_whitespace().next();
        let first_b = b.split_whitespace().next();
        first_a == first_b
    }
}
```

### Why consecutive only

We only compare adjacent lines, not all-pairs. Reasons:
- Performance: O(n) not O(n²)
- Accuracy: repetitive output is almost always consecutive
- Memory: no need to remember all lines, just the previous one

Non-consecutive duplicates (same message at minute 1 and minute 5) are rare and usually meaningful enough to show both times.

## Memory Management

The dedup engine needs bounded memory. Safeguards:

```
1. Max groups: 1000
   When exceeded, flush the oldest group (by first_seen)

2. Max template length: 500 chars
   Longer templates are truncated before storage

3. Count threshold flush: 5 (configurable)
   Once a group hits N, emit it and remove from tracking
   This prevents a single repetitive pattern from consuming memory

4. Periodic flush: every 5s
   Prevents accumulation during long-running processes
```

## Integration with Emit Buffer

The dedup engine is owned by the emit buffer. The flow:

```
Line classified as Noise(Dedup)
    │
    ▼
EmitBuffer.accept()
    │
    ├─── sends to DedupEngine.ingest()
    │
    ▼
On flush trigger:
    │
    ├─── DedupEngine.flush_all() → Vec<String>
    │
    ▼
Formatted dedup lines emitted with ~ prefix (warnings)
or as plain collapsed lines (neutral noise)
```

The `~` prefix is used when the deduped lines were classified as warnings by the grammar (e.g., `npm warn`). Neutral noise (e.g., "Downloading packages") gets `...` or plain indented text.
