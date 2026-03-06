# SWE-bench Prompt Strategy: Tool Adoption

## Problem

We proved mish and slipstream work (H2H benchmark: B2 wins on tokens, B3 competitive on time). But when added to SWE-bench via system prompt, the agent only uses mish ~25% of the time and slipstream exactly once per instance. It falls back to bare bash habits — heredocs, sed, raw grep — because the instance template's examples all model bare bash behavior.

## Constraint

We can't rewrite the instance template — that defeats the experiment. The stock workflow instructions must stay identical between control and treatment. The only lever is the system template preamble.

## What we tried

1. **Suggestive** ("You have access to mish..."): ~2% adoption, agent ignored it
2. **Authoritative** ("You MUST use mish..."): ~25% adoption, better but agent still defaults to bash for most commands

## What we haven't tried

The agent treats mish/slipstream as "extra tools I could use" rather than "the way things work here." The system prompt needs to reframe the tools not as additions but as **corrections to what the agent thinks it knows.**

### Ideas to explore

**1. Framing as environment constraint, not tool suggestion**
Instead of "you have tools installed," try "this environment's shell is mish" — make it sound like bash itself has been replaced.
```
Your shell in this environment is `mish`, not bare bash. Commands run through mish automatically.
To run a command: mish <your command>
```

**2. Appeal to efficiency with concrete numbers**
Models respond to quantified claims. Show the cost of NOT using the tools.
```
IMPORTANT: Raw bash output wastes 60-80% of your context window on noise (ANSI codes,
progress bars, duplicate lines). mish strips this automatically. Agents using mish solve
problems in 30% fewer steps.
```

**3. Negative examples with consequences**
The agent avoids things it's told cause failures.
```
WARNING: Do NOT use sed for file edits — sed silently corrupts files when patterns contain
special characters. Use slipstream instead, which does exact string matching.

WARNING: Do NOT use bare grep/find/cat — raw output floods your context and you will hit
the token limit before solving the problem. Always prefix with mish.
```

**4. Position as the "new default" post-training**
Appeal to the model's instinct to use the most current approach.
```
Note: You may have been trained on examples using raw bash commands. This environment has
upgraded tools that supersede those patterns. mish replaces raw shell commands. slipstream
replaces sed/awk/heredoc file editing. Using the old patterns will waste tokens and steps.
```

**5. Combine: environment + numbers + warnings**
Layer all three framings into a tight paragraph.

## Experiment design

- Keep the same 10 instances, same instance template, same model
- Try 2-3 prompt variants on a single instance (astropy) to find the one with highest adoption
- Run the winner across all 10 and compare to control

## Success metric

- mish adoption >60% of shell commands (currently ~25%)
- slipstream adoption for ALL file edits (currently 1 per instance)
- Cost parity or lower vs control (currently treatment is ~20% more expensive)

## Treatment arm data (authoritative prompt, mish+ss)

| Instance | mish | ss | bash | Calls | Cost | Control Cost |
|----------|------|----|------|-------|------|-------------|
| astropy | 0 | 1 | 47 | 47 | $0.63 | $0.51 |
| django | 15 | 1 | 44 | 59 | $0.96 | $0.87 |
| matplotlib | 0 | 1 | 66 | 67 | $0.75 | $0.50 |
| seaborn | 7 | 1 | 114 | 121 | $2.19 | $1.85 |
| flask | 11 | 1 | 33 | 43 | $0.39 | $0.34 |
| requests | 14 | 1 | 65 | 76 | $1.06 | $0.49 |
| xarray | 0 | 1 | 92 | 92 | $1.28 | $0.94 |
| pylint | 0 | 1 | 71 | 71 | $0.93 | $1.46 |
| pytest | 0 | 1 | 53 | 49 | $0.51 | $0.50 |
| scikit-learn | 7 | 1 | 51 | 59 | $0.61 | $0.87 |

**Total: $9.31 vs $8.33 control (+11.8%)**

mish adoption: 54/700 commands (8%). Slipstream: exactly 1 call per instance.
5/10 instances used mish zero times. The agent takes more steps overall, negating any per-step token savings.

## Control arm data (baseline)

| Instance | Calls | Cost |
|----------|-------|------|
| astropy | 39 | $0.51 |
| django | 56 | $0.87 |
| matplotlib | 52 | $0.50 |
| seaborn | 113 | $1.85 |
| flask | 35 | $0.34 |
| requests | 46 | $0.49 |
| xarray | 88 | $0.94 |
| pylint | 107 | $1.46 |
| pytest | 49 | $0.50 |
| scikit-learn | 73 | $0.87 |
