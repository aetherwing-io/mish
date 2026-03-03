# mish Integration Testing: Metrics Capture Protocol

## What to capture on EVERY command execution

For each command run through mish, log a structured record:

```
{
  "command": "pytest tests/ -v",
  "category": "Condense",                  // mish's classification
  "grammar_matched": "pytest",             // which TOML grammar, or "none/default"
  
  "raw": {
    "stdout_bytes": 14832,
    "stderr_bytes": 420,
    "stdout_lines": 347,
    "stderr_lines": 8,
    "raw_tokens": 4210                     // tokenize the raw output (cl100k_base or claude tokenizer)
  },
  
  "squashed": {
    "output_bytes": 312,
    "output_lines": 8,
    "squashed_tokens": 89,
    "digest_tokens": 22                    // process digest appended
  },
  
  "derived": {
    "compression_ratio": 0.021,            // squashed_tokens / raw_tokens
    "token_reduction": 4121,               // raw - squashed
    "token_reduction_pct": 97.9,
    "lines_kept": 8,
    "lines_stripped": 339
  },
  
  "classification": {
    "noise_lines": 312,                    // matched noise patterns
    "signal_lines": 27,                    // matched signal/keep patterns  
    "outcome_lines": 3,                    // matched outcome/promote patterns
    "unclassified_lines": 5,              // fell through to default heuristic
    "deduplicated_groups": 4              // template dedup (e.g., "Downloading x10")
  },
  
  "quality": {
    "exit_code": 1,
    "semantic_completeness": null,         // fill in manually: did the summary preserve what the agent needs?
    "signal_lost": null,                   // fill in manually: any critical info stripped?
    "signal_lost_detail": null             // if yes, what was it and which rule caught it?
  },

  "timing": {
    "command_wall_ms": 12340,
    "squash_wall_ms": 2,                   // squasher processing time
    "grammar_load_ms": 0                   // if grammar was already cached
  }
}
```

## Test matrix

Run each of these and capture the record above. These represent the commands agents actually run on SWE-bench:

### High-noise (where mish should dominate)
- `pytest tests/ -v` on a Django repo (or any large test suite)
- `pytest tests/specific_test.py -v` with 2-3 failures among many passes
- `pip install -e .` on a project with 30+ dependencies
- `pip install -r requirements.txt` with deprecation warnings
- `git log --oneline -50`
- `git diff HEAD~3` on a repo with 200+ changed lines
- `python -m flake8 src/` with 40+ style warnings

### Medium-noise (mixed signal)
- `git status` with 5-10 modified files
- `git diff path/to/specific_file.py` (small, targeted diff)
- `python reproduce_bug.py` that throws a traceback
- `grep -rn "function_name" src/` with 15+ matches
- `find . -name "*.py" -path "*/tests/*"` 

### Low-noise (mostly signal, squasher should be near-passthrough)
- `cat src/specific_file.py` (small file, <50 lines)
- `python -c "import sys; print(sys.version)"`
- `pwd && ls`
- `echo $VIRTUAL_ENV`

### Edge cases (where squasher might fail)
- `pytest` with ONLY failures (no passing tests to strip — is the output still good?)
- `python script.py` with interleaved stdout/stderr
- Command that produces binary-looking output
- Command that hangs and gets killed (timeout behavior)
- Empty output (exit code 0, no stdout, no stderr)

## Aggregation targets

After running the full matrix, compute:

```
Overall:
  - Total raw tokens across all commands: ___
  - Total squashed tokens across all commands: ___
  - Aggregate compression ratio: ___
  - Aggregate token savings: ___

By category:
  - High-noise avg compression ratio: ___
  - Medium-noise avg compression ratio: ___  
  - Low-noise avg compression ratio: ___

By grammar:
  - pytest: avg compression ratio, signal preservation rate
  - pip: avg compression ratio, signal preservation rate
  - git: avg compression ratio, signal preservation rate
  - default/ungrammar'd: avg compression ratio

Quality:
  - Commands where signal was lost: ___ / total
  - Commands where squasher was near-passthrough (ratio > 0.8): ___
  - Commands where compression ratio < 0.1 (>90% reduction): ___

The money number (for the paper):
  - "Across N commands representative of SWE-bench agent workflows, 
     mish reduced output tokens from X to Y (Z% reduction) with 
     N/N semantic completeness (zero signal loss)."
```

## Manual quality review

For every command where compression ratio < 0.2 (aggressive squashing), manually verify:

1. Read the raw output
2. Read the squashed output  
3. Answer: "If I were an agent trying to fix a bug, does the squashed output contain everything I need to decide my next action?"
4. If NO: what's missing? Which grammar rule removed it? Can the rule be fixed?

This is the credibility data. The compression numbers sell the paper. The quality review defends it.
