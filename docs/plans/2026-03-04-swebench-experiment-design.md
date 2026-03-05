# SWE-bench Experiment: mish vs bare bash

**Date:** 2026-03-04
**Status:** Design approved

## Hypothesis

mish's output compression (squash + dedup + progress removal), error enrichment, and preflight injection allow an LLM agent to solve more SWE-bench problems under tight context constraints.

## Experimental Setup

### Harness

Fork of [mini-SWE-agent](https://github.com/SWE-agent/mini-swe-agent) — the ~100-line ReAct agent used for the SWE-bench Verified "Bash Only" leaderboard.

### Benchmark

SWE-bench Verified (500 curated instances from 12 Python repos). Initial smoke test: 10 instances.

### Model

Match the bash-only leaderboard's current model (e.g., Claude Sonnet 4.5 via LiteLLM).

### Arms

| Arm | Shell | Interpreter | Config |
|-----|-------|-------------|--------|
| Control | bare bash | `["bash", "-c"]` | Stock mini-SWE-agent `swebench.yaml` |
| Treatment | mish | `["mish"]` | Same config, interpreter swapped |

Both arms use the **identical observation template** — the only variable is the shell.

### Constraints

- **32K context window cap:** Add a `token_limit` check in the agent's `query()` method. When cumulative input tokens exceed 32K, raise `LimitsExceeded`.
- **Total token tracking:** LiteLLM returns `usage` per call. Log per-step, aggregate per-instance.

### Instance Selection (Smoke Test)

Pick 10 instances spanning different repos and difficulty levels. Use the same 10 for both arms for paired comparison.

## Integration: mish in Docker

### Binary

Cross-compile `x86_64-unknown-linux-gnu` (static or glibc-linked). Add to the [v0.1.0 release](https://github.com/aetherwing-io/mish/releases/tag/v0.1.0) alongside the existing `aarch64` builds. Also update `.github/workflows/` for future releases.

### Installation

During the environment's `setup()` phase, before any agent steps:

```bash
curl -sL https://github.com/aetherwing-io/mish/releases/download/v0.1.0/mish-v0.1.0-x86_64-unknown-linux-gnu.tar.gz \
  | tar xz -C /usr/local/bin/
```

Bundle `grammars/` in the tarball. mish discovers grammars relative to its binary path.

### Interpreter Swap

mini-SWE-agent's Docker environment config has `interpreter` (defaults to `["bash", "-c"]`). For treatment: set to `["mish"]`.

The `execute()` method in `docker.py` builds the command as:

```python
cmd.extend([self.container_id, *self.config.interpreter, command])
```

So control runs `docker exec <id> bash -c <command>` and treatment runs `docker exec <id> mish <command>`.

### No Config Required

mish's defaults are sane: squasher, dedup, enrichment all enabled out of the box. No `mish.toml` needed.

## Observation Template

**Kept identical for both arms.** Stock `swebench.yaml` template:

- Output < 10K chars: full `<output>...</output>`
- Output >= 10K chars: head/tail truncation (5K + 5K) with warning

mish's compressed output hits this template already shorter, so it fits under 10K more often. The template acts as a second safety net, not the primary compression.

## Metrics

### Primary

| Metric | Source | What it measures |
|--------|--------|-----------------|
| Resolve rate | SWE-bench test suite | Did the agent fix the issue? |
| Total tokens per instance | LiteLLM usage | Context efficiency |
| Steps per instance | Agent step counter | How many actions needed? |
| Cost per instance | LiteLLM cost tracking | Dollar cost |

### mish-specific (treatment arm only)

Scraped from audit log (`~/.local/share/mish/audit.log`) inside the Docker container after each instance.

| Metric | Source | What it measures |
|--------|--------|-----------------|
| `raw_bytes` / `squashed_bytes` | SquasherReport | Bytes saved by compression |
| `compression_ratio` | SquasherReport | `squashed / raw` (lower = more compression) |
| `lines_in` / `lines_out` | PipelineMetrics | Line-level compression |
| `vte_stripped` | PipelineMetrics | ANSI escapes removed |
| `progress_stripped` | PipelineMetrics | Progress bars removed |
| `dedup_groups` / `dedup_absorbed` | PipelineMetrics | Duplicate lines absorbed |
| `oreo_suppressed` | PipelineMetrics | Lines dropped by Oreo truncation |
| `noise_lines` / `signal_lines` | EmitMetrics | Classification breakdown |
| `wall_ms` / `squash_ms` | SquasherReport | Execution and squasher timing |

### Analysis Questions

- Does mish improve resolve rate under 32K context cap?
- How many tokens does mish save per instance on average?
- Do instances with higher compression ratios correlate with resolution?
- Which squasher stages contribute most (dedup? progress? Oreo?)?
- Does error enrichment reduce wasted steps after failures?

## Deliverables

1. **x86_64 binary** — cross-compiled, added to v0.1.0 release
2. **Release workflow update** — `.github/workflows/` builds x86_64 on future releases
3. **mini-SWE-agent fork** — with mish integration
4. **Two YAML configs** — `swebench_control.yaml` and `swebench_mish.yaml`
5. **Token limit patch** — 32K cap in agent's `query()` method
6. **Results analysis script** — parses trajectories + mish audit logs, produces comparison table
7. **10-instance smoke test results** — paired comparison with all metrics

## Scaling Plan

After smoke test validates the harness:

- Run full 500 instances (both arms)
- Budget: ~$1500-3000 total
- Runtime: ~1 hour on 32-core machine (per Epoch AI's Docker optimization)
- Publishable results for the mish README/landing page
