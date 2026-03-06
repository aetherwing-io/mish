# SWE-bench A/B Test v2 — Results

**Date**: 2026-03-05
**Versions**: mish v0.3.2, slipstream v0.3.0, Sonnet 4.6 (claude-sonnet-4-6)
**Framework**: mini-swe-agent 2.2.6, SWE-bench Verified (10 instances)
**Configs**: `src/minisweagent/config/benchmarks/swebench_{mish,control}.yaml`

## Summary

| Metric | Treatment (mish+ss) | Control (bare bash) |
|--------|--------------------|--------------------|
| Resolved | **8/10 (80%)** | 7/9 (78%) |
| Submitted | 10/10 | 9/10 |
| Total cost | **$5.98** | $6.79 |
| Wall clock | 59 min | 67 min |
| Cost delta | — | **-12%** |

Treatment wins on both axes: more resolves for less money.

## Per-Instance Breakdown

| Instance | Treat $ | Ctrl $ | Delta | Treat | Ctrl | mish% | ss% |
|----------|---------|--------|-------|-------|------|-------|-----|
| astropy-12907 | $0.28 | $0.22 | +25% | RESOLVED | RESOLVED | 60% | 32% |
| django-10097 | $0.43 | $0.67 | **-36%** | unresolved | unresolved | 71% | 26% |
| matplotlib-13989 | $0.41 | $0.12 | +243% | RESOLVED | RESOLVED | 80% | 17% |
| seaborn-3069 | $2.42 | $3.01 | **-20%** | **RESOLVED** | LimitsExceeded | 90% | 9% |
| flask-5014 | $0.14 | $0.18 | **-23%** | RESOLVED | RESOLVED | 77% | 18% |
| requests-1142 | $0.23 | $0.40 | **-42%** | RESOLVED | RESOLVED | 79% | 17% |
| xarray-2905 | $0.73 | $0.88 | **-18%** | RESOLVED | RESOLVED | 75% | 24% |
| pylint-4551 | $0.83 | $0.91 | **-9%** | unresolved | unresolved | 42% | 24% |
| pytest-10051 | $0.33 | $0.22 | +51% | RESOLVED | RESOLVED | 65% | 26% |
| scikit-learn-10297 | $0.18 | $0.17 | +5% | RESOLVED | RESOLVED | 69% | 23% |

## Key Findings

### 1. Squasher earns its keep on heavy instances

The biggest cost savings came from verbose, high-step instances where mish's dedup/truncation compresses output:

- **requests**: -42% ($0.23 vs $0.40)
- **django**: -36% ($0.43 vs $0.67)
- **flask**: -23% ($0.14 vs $0.18)
- **seaborn**: -20% ($2.42 vs $3.01) — and treatment resolved it while control hit LimitsExceeded

### 2. Treatment over-thoroughness on easy instances

Three instances cost more in treatment (matplotlib +243%, pytest +51%, astropy +25%). All are low-cost instances where the control solved quickly. The treatment agent was more methodical — running extra pytest suites, git stash validation, multiple file reads — producing the same fix but in more steps.

**matplotlib example**: Both produced identical 1-line fix (`hist_kwargs = dict(density=density)` → `hist_kwargs["density"] = density`). Control: 21 steps, $0.12. Treatment: 41 steps, $0.41. Extra steps were 5 pytest invocations, git stash baseline test, and multiple linecache file reads.

This is a prompt tuning issue, not a mish issue. The detailed "Recommended Workflow" section encourages more verification than needed on simple problems.

### 3. Tool adoption: 42-90% mish, 9-32% slipstream

Massive improvement over v1 (Sonnet 3.5: 8-14% adoption). Sonnet 4.6 + the siege prompt drives consistent adoption. Bare bash calls averaged 1-3 per instance except pylint (21 bare).

### 4. Seaborn: treatment's decisive win

The hardest instance in the set. Control burned $3.01 (146 commands) and hit LimitsExceeded without submitting. Treatment resolved it for $2.42 (116 commands). The squasher's output compression kept the agent under budget.

## Bugs & Issues Found

### Slipstream: file.write fails on new files

When the agent tries to create a new file via `slipstream exec --files test.py --ops '[{"method":"file.write",...}]'`, it fails with "No such file or directory". Requires `touch` first. The prompt example for file creation uses `file.write` with a string `content` field, but the actual API requires:
- File must already exist
- `content` must be an array of lines (not a string)
- `start` and `end` fields are required

This caused a 7-attempt failure spiral on pylint (steps 44-53) where the agent tried every permutation before checking `slipstream --help`.

### Linecache anti-pattern: no line-range read in prompt

On large files (matplotlib: 7000+ lines), the agent needs partial reads. The prompt only shows `--read-all`. Without a line-range example, the agent falls back to:
```bash
mish python3 -c "import linecache; for i in range(6650, 6720): ..."
```
This was done 5 separate times at different ranges — 5 API round-trips for what should be 1-2 slipstream calls.

**Fix**: Add line-range read example to prompt and/or add `--lines` flag to slipstream.

### Python environment discovery

`mish python3` uses system python, which doesn't have testbed dependencies (astroid, etc.). Agent discovers `/opt/miniconda3/envs/testbed/bin/python3` and drops mish prefix entirely for the rest of the session. 12 bare bash calls on pylint alone from this.

**Fix**: Prompt should specify `mish /opt/miniconda3/envs/testbed/bin/python3` or set up a PATH alias in startup_commands.

## Comparison to v1

| Metric | v1 (Sonnet 3.5) | v2 (Sonnet 4.6) |
|--------|-----------------|-----------------|
| Treatment cost | $6.43 | $5.98 |
| Control cost | $5.99 | $6.79 |
| Delta | +7% (treatment worse) | **-12% (treatment better)** |
| mish adoption | 87% | 71% avg |
| Resolve rate | not evaluated | 80% treat / 78% ctrl |
| Key bugs | squasher destroying code | slipstream write API mismatch |

v1 treatment was 7% MORE expensive due to squasher destroying code/diffs (182 retry spirals). With that fixed in v0.3.1+, the squasher is now a net win.

## Next Steps

1. **Fix slipstream file.write** — support string content, auto-create files
2. **Add line-range read** to slipstream or prompt
3. **Fix python env** — add `ln -s /opt/miniconda3/envs/testbed/bin/python3 /usr/local/bin/python3` to startup_commands
4. **Tune prompt thoroughness** — reduce over-verification on easy instances (maybe remove "test edge cases" step)
5. **Scale to 50+ instances** once bugs are fixed
6. **Evaluate django/pylint failures** — are these solvable or inherently hard?
