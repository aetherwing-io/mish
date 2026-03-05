# SWE-bench Experiment Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Run a paired A/B test of mish vs bare bash on SWE-bench Verified using mini-SWE-agent, measuring resolve rate and token efficiency under a 32K context cap.

**Architecture:** Fork mini-SWE-agent, add a `token_limit` gate, swap the Docker interpreter to mish for the treatment arm. Cross-compile mish for x86_64 Linux and upload to the GitHub release. Run 10 instances as a smoke test.

**Tech Stack:** Python (mini-SWE-agent), Rust (mish cross-compilation), Docker, LiteLLM, GitHub Actions/Releases

**Design doc:** `docs/plans/2026-03-04-swebench-experiment-design.md`

---

### Task 1: Cross-compile mish for x86_64 Linux

We need the binary for SWE-bench Docker containers (Debian x86_64).

**Files:**
- None modified — this is a build + upload task

**Step 1: Install the x86_64 Linux target**

Run: `rustup target add x86_64-unknown-linux-gnu`
Expected: target installed (or already present)

**Step 2: Install the cross-compilation toolchain**

Run: `brew install messense/macos-cross-toolchains/x86_64-unknown-linux-gnu`

This gives us the GCC cross-linker for targeting x86_64 Linux from macOS.

**Step 3: Configure cargo for cross-linking**

Create/update `~/.cargo/config.toml` (or project-local `.cargo/config.toml`):

```toml
[target.x86_64-unknown-linux-gnu]
linker = "x86_64-unknown-linux-gnu-gcc"
```

**Step 4: Cross-compile the release binary**

Run: `cargo build --release --target x86_64-unknown-linux-gnu`
Expected: binary at `target/x86_64-unknown-linux-gnu/release/mish`

**Step 5: Verify the binary**

Run: `file target/x86_64-unknown-linux-gnu/release/mish`
Expected: `ELF 64-bit LSB ... x86-64 ... dynamically linked`

**Step 6: Package the tarball with grammars**

```bash
mkdir -p /tmp/mish-package/grammars
cp target/x86_64-unknown-linux-gnu/release/mish /tmp/mish-package/
cp -r grammars/* /tmp/mish-package/grammars/
cd /tmp/mish-package && tar czf /tmp/mish-v0.1.0-x86_64-unknown-linux-gnu.tar.gz mish grammars/
```

**Step 7: Upload to the existing v0.1.0 release**

Run: `gh release upload v0.1.0 /tmp/mish-v0.1.0-x86_64-unknown-linux-gnu.tar.gz --repo aetherwing-io/mish`
Expected: asset added to existing release

**Step 8: Verify the upload**

Run: `gh release view v0.1.0 --repo aetherwing-io/mish --json assets --jq '.assets[].name'`
Expected: `mish-v0.1.0-x86_64-unknown-linux-gnu.tar.gz` appears in the list

**Step 9: Commit**

No code changes — just the release upload. No commit needed.

---

### Task 2: Fork and clone mini-SWE-agent

**Files:**
- New repo: `~/projects/mini-swe-agent` (fork of `SWE-agent/mini-swe-agent`)

**Step 1: Fork the repo**

Run: `gh repo fork SWE-agent/mini-swe-agent --clone --remote --org aetherwing-io`
Expected: forked to `aetherwing-io/mini-swe-agent`, cloned locally

**Step 2: Verify the clone**

Run: `ls ~/projects/mini-swe-agent/src/minisweagent/`
Expected: `agents/`, `environments/`, `models/`, `config/`, etc.

**Step 3: Install dependencies**

```bash
cd ~/projects/mini-swe-agent
pip install -e ".[dev]"
```

Expected: mini-swe-agent installed in editable mode

**Step 4: Verify it runs**

Run: `mini --help`
Expected: help output from mini-swe-agent CLI

---

### Task 3: Add 32K token limit to the agent

The stock agent has `step_limit` and `cost_limit` but no token budget cap. We add one.

**Files:**
- Modify: `src/minisweagent/agents/default.py` (the `query()` method, around line 95)

**Step 1: Write the failing test**

Create: `tests/test_token_limit.py`

```python
import pytest
from unittest.mock import MagicMock, patch
from minisweagent.agents.default import DefaultAgent, AgentConfig
from minisweagent.exceptions import LimitsExceeded


def make_agent(token_limit=32000):
    """Create an agent with mocked model and env."""
    model = MagicMock()
    env = MagicMock()
    env.get_template_vars.return_value = {}

    # Model returns a message with usage stats
    model.query.return_value = {
        "role": "assistant",
        "content": "test",
        "extra": {
            "cost": 0.001,
            "actions": [],
            "usage": {"prompt_tokens": 20000, "completion_tokens": 500},
        },
    }
    model.format_message.side_effect = lambda **kwargs: kwargs
    model.get_template_vars.return_value = {}
    model.format_observation_messages.return_value = []

    agent = DefaultAgent(
        model=model,
        env=env,
        system_template="system",
        instance_template="instance",
        token_limit=token_limit,
    )
    return agent


def test_token_limit_raises_when_exceeded():
    agent = make_agent(token_limit=32000)
    # Simulate accumulated tokens
    agent.cumulative_input_tokens = 33000
    with pytest.raises(LimitsExceeded):
        agent.query()


def test_token_limit_zero_means_unlimited():
    agent = make_agent(token_limit=0)
    agent.cumulative_input_tokens = 999999
    # Should not raise — 0 means no limit
    result = agent.query()
    assert result is not None
```

**Step 2: Run test to verify it fails**

Run: `cd ~/projects/mini-swe-agent && pytest tests/test_token_limit.py -v`
Expected: FAIL — `AgentConfig` doesn't accept `token_limit`, `cumulative_input_tokens` doesn't exist

**Step 3: Add `token_limit` to AgentConfig and tracking to DefaultAgent**

In `src/minisweagent/agents/default.py`:

1. Add to `AgentConfig`:
```python
token_limit: int = 0
"""Stop agent after exceeding this many cumulative input tokens. 0 = unlimited."""
```

2. In `DefaultAgent.__init__`, add:
```python
self.cumulative_input_tokens = 0
```

3. In `DefaultAgent.query()`, after the model call (after `self.cost += ...`), add:
```python
usage = message.get("extra", {}).get("usage", {})
self.cumulative_input_tokens += usage.get("prompt_tokens", 0)
if 0 < self.config.token_limit <= self.cumulative_input_tokens:
    raise LimitsExceeded(
        {
            "role": "exit",
            "content": "TokenLimitExceeded",
            "extra": {"exit_status": "TokenLimitExceeded", "submission": ""},
        }
    )
```

**Step 4: Run test to verify it passes**

Run: `cd ~/projects/mini-swe-agent && pytest tests/test_token_limit.py -v`
Expected: 2 passed

**Step 5: Commit**

```bash
cd ~/projects/mini-swe-agent
git add src/minisweagent/agents/default.py tests/test_token_limit.py
git commit -m "feat: add token_limit to agent config for context budget experiments"
```

---

### Task 4: Create the mish treatment YAML config

**Files:**
- Copy: `src/minisweagent/config/benchmarks/swebench.yaml` → `src/minisweagent/config/benchmarks/swebench_mish.yaml`
- Copy: `src/minisweagent/config/benchmarks/swebench.yaml` → `src/minisweagent/config/benchmarks/swebench_control.yaml`

**Step 1: Create the control config**

Copy `swebench.yaml` to `swebench_control.yaml`. Add token_limit:

```yaml
agent:
  step_limit: 250
  cost_limit: 3.
  token_limit: 32000
```

Everything else stays the same.

**Step 2: Create the treatment config**

Copy `swebench_control.yaml` to `swebench_mish.yaml`. Change the environment interpreter:

In the `environment` section, add or modify:

```yaml
environment:
  interpreter:
    - "mish"
```

And add a setup command to install mish in the Docker container:

```yaml
environment:
  setup_commands:
    - "curl -sL https://github.com/aetherwing-io/mish/releases/download/v0.1.0/mish-v0.1.0-x86_64-unknown-linux-gnu.tar.gz | tar xz -C /usr/local/bin/"
```

**Step 3: Verify configs parse**

Run: `cd ~/projects/mini-swe-agent && python -c "import yaml; print(yaml.safe_load(open('src/minisweagent/config/benchmarks/swebench_control.yaml'))['agent']['token_limit'])"`
Expected: `32000`

Run: `cd ~/projects/mini-swe-agent && python -c "import yaml; print(yaml.safe_load(open('src/minisweagent/config/benchmarks/swebench_mish.yaml'))['environment']['interpreter'])"`
Expected: `['mish']`

**Step 4: Commit**

```bash
cd ~/projects/mini-swe-agent
git add src/minisweagent/config/benchmarks/swebench_control.yaml src/minisweagent/config/benchmarks/swebench_mish.yaml
git commit -m "feat: add control and mish configs for SWE-bench A/B experiment"
```

---

### Task 5: Add mish metrics scraping

After each instance, extract mish's audit log from the Docker container.

**Files:**
- Create: `scripts/scrape_mish_metrics.py`

**Step 1: Write the scraping script**

```python
#!/usr/bin/env python3
"""Scrape mish audit log from a Docker container after an instance run."""

import json
import subprocess
import sys
from pathlib import Path


def scrape_audit_log(container_id: str, output_path: str) -> dict:
    """Extract and parse mish audit log from container."""
    result = subprocess.run(
        ["docker", "exec", container_id, "cat", "/root/.local/share/mish/audit.log"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return {"error": "no audit log found", "stderr": result.stderr}

    records = []
    for line in result.stdout.strip().split("\n"):
        if not line.strip():
            continue
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            continue

    # Aggregate metrics
    total_raw_bytes = sum(r.get("raw_bytes", 0) for r in records)
    total_squashed_bytes = sum(r.get("squashed_bytes", 0) for r in records)
    total_commands = len(records)
    avg_compression = (
        total_squashed_bytes / total_raw_bytes if total_raw_bytes > 0 else 1.0
    )

    summary = {
        "total_commands": total_commands,
        "total_raw_bytes": total_raw_bytes,
        "total_squashed_bytes": total_squashed_bytes,
        "bytes_saved": total_raw_bytes - total_squashed_bytes,
        "avg_compression_ratio": round(avg_compression, 4),
        "records": records,
    }

    if output_path:
        Path(output_path).write_text(json.dumps(summary, indent=2))

    return summary


if __name__ == "__main__":
    container_id = sys.argv[1]
    output_path = sys.argv[2] if len(sys.argv) > 2 else ""
    result = scrape_audit_log(container_id, output_path)
    print(json.dumps(result, indent=2))
```

**Step 2: Verify it runs (dry run)**

Run: `cd ~/projects/mini-swe-agent && python scripts/scrape_mish_metrics.py nonexistent_container /dev/null`
Expected: `{"error": "no audit log found", ...}` (graceful failure)

**Step 3: Commit**

```bash
cd ~/projects/mini-swe-agent
git add scripts/scrape_mish_metrics.py
git commit -m "feat: add mish audit log scraping script for metrics collection"
```

---

### Task 6: Create the results analysis script

**Files:**
- Create: `scripts/analyze_results.py`

**Step 1: Write the analysis script**

```python
#!/usr/bin/env python3
"""Compare control vs treatment results from SWE-bench runs."""

import json
import sys
from pathlib import Path


def load_trajectories(results_dir: str) -> list[dict]:
    """Load all trajectory JSON files from a results directory."""
    trajectories = []
    for path in sorted(Path(results_dir).glob("*.json")):
        data = json.loads(path.read_text())
        trajectories.append(data)
    return trajectories


def summarize_arm(trajectories: list[dict], label: str) -> dict:
    """Summarize metrics for one arm of the experiment."""
    resolved = sum(1 for t in trajectories if t.get("info", {}).get("exit_status") == "submitted")
    total_cost = sum(t.get("info", {}).get("model_stats", {}).get("instance_cost", 0) for t in trajectories)
    total_calls = sum(t.get("info", {}).get("model_stats", {}).get("api_calls", 0) for t in trajectories)

    return {
        "label": label,
        "instances": len(trajectories),
        "resolved": resolved,
        "resolve_rate": round(resolved / len(trajectories) * 100, 1) if trajectories else 0,
        "total_cost": round(total_cost, 4),
        "avg_cost": round(total_cost / len(trajectories), 4) if trajectories else 0,
        "total_api_calls": total_calls,
        "avg_api_calls": round(total_calls / len(trajectories), 1) if trajectories else 0,
    }


def print_comparison(control: dict, treatment: dict):
    """Print a side-by-side comparison table."""
    print(f"\n{'Metric':<25} {'Control (bash)':<20} {'Treatment (mish)':<20} {'Delta':<15}")
    print("-" * 80)

    for key in ["instances", "resolved", "resolve_rate", "total_cost", "avg_cost", "total_api_calls", "avg_api_calls"]:
        c_val = control[key]
        t_val = treatment[key]
        if isinstance(c_val, float):
            delta = f"{t_val - c_val:+.2f}"
        else:
            delta = f"{t_val - c_val:+d}" if isinstance(c_val, int) else ""
        unit = "%" if key == "resolve_rate" else "$" if "cost" in key else ""
        print(f"{key:<25} {str(c_val) + unit:<20} {str(t_val) + unit:<20} {delta:<15}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(f"Usage: {sys.argv[0]} <control_results_dir> <treatment_results_dir>")
        sys.exit(1)

    control = summarize_arm(load_trajectories(sys.argv[1]), "control")
    treatment = summarize_arm(load_trajectories(sys.argv[2]), "treatment")
    print_comparison(control, treatment)
```

**Step 2: Commit**

```bash
cd ~/projects/mini-swe-agent
git add scripts/analyze_results.py
git commit -m "feat: add results comparison script for A/B analysis"
```

---

### Task 7: Select 10 smoke test instances

Pick 10 SWE-bench Verified instances spanning different repos and difficulty.

**Files:**
- Create: `configs/smoke_test_instances.txt`

**Step 1: Download the instance list**

```bash
cd ~/projects/mini-swe-agent
python -c "
from datasets import load_dataset
ds = load_dataset('princeton-nlp/SWE-bench_Verified', split='test')
# Pick 10 spanning different repos
repos = {}
for item in ds:
    repo = item['repo']
    if repo not in repos:
        repos[repo] = item['instance_id']
    if len(repos) >= 10:
        break
for iid in repos.values():
    print(iid)
" > configs/smoke_test_instances.txt
```

**Step 2: Verify**

Run: `wc -l configs/smoke_test_instances.txt`
Expected: 10 lines

**Step 3: Commit**

```bash
cd ~/projects/mini-swe-agent
git add configs/smoke_test_instances.txt
git commit -m "feat: add 10 smoke test instance IDs for initial experiment"
```

---

### Task 8: Run the smoke test — control arm

**Step 1: Run control arm**

```bash
cd ~/projects/mini-swe-agent
mini run-batch \
  --config src/minisweagent/config/benchmarks/swebench_control.yaml \
  --instances configs/smoke_test_instances.txt \
  --output-dir results/control/ \
  --dataset princeton-nlp/SWE-bench_Verified
```

Expected: 10 trajectory JSON files in `results/control/`

**Step 2: Verify trajectories**

Run: `ls results/control/*.json | wc -l`
Expected: 10

---

### Task 9: Run the smoke test — treatment arm

**Step 1: Run treatment arm**

```bash
cd ~/projects/mini-swe-agent
mini run-batch \
  --config src/minisweagent/config/benchmarks/swebench_mish.yaml \
  --instances configs/smoke_test_instances.txt \
  --output-dir results/treatment/ \
  --dataset princeton-nlp/SWE-bench_Verified
```

Expected: 10 trajectory JSON files in `results/treatment/`

**Step 2: Scrape mish metrics from each container**

(This may need to be integrated into the batch runner or done manually per-instance.)

---

### Task 10: Analyze results

**Step 1: Run comparison**

Run: `python scripts/analyze_results.py results/control/ results/treatment/`

Expected: comparison table showing resolve rate, tokens, cost, steps for both arms.

**Step 2: Document findings**

Create `results/smoke_test_report.md` with the comparison table and observations.

**Step 3: Commit**

```bash
cd ~/projects/mini-swe-agent
git add results/ scripts/
git commit -m "results: smoke test — mish vs bash on 10 SWE-bench instances"
```
