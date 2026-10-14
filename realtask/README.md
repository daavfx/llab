# realtask -- REALTASK grader harness (Rust)

The M4 quality axis: a zero-dependency Rust replacement for the legacy python
task harness. It runs the standing 5-task suite against a live llama-server
and grades with fixed per-task graders, writing `scores.json`.

## Why it exists

The legacy harness (python) scored models but kept a runtime dependency and a
config-drift risk (the "0/10 thinking-model" artifact). This is the same
contract in Rust: fixed tasks, fixed graders, deterministic (temperature 0,
seed 42), append-only rows.

## Tasks + graders

| task | capability | full marks (2) |
|---|---|---|
| task_1_code_fix | fix an off-by-one comparison | corrected comparison, bug removed |
| task_2_reasoning | staged-rates arithmetic | exact `ANSWER: 8:40` |
| task_3_json_extract | strict JSON from messy prose | one clean object, 4 exact fields |
| task_4_summarize | 3-bullet summary | exactly 3 bullets |
| task_5_tool_plan | numbered tool plan | 4 numbered steps + PLAN_END |

## Usage

```
cargo build --release
target\release\realtask.exe --port 8092 \
    --tasks <path>\04_task_suite_harness\tasks \
    --out   scores.json --label my-run [--no-think]
```

- `--no-think`: sends `chat_template_kwargs.enable_thinking:false`. Required for
  thinking models (qwen3.5, bonsai) or the whole budget burns into
  `reasoning_content` and content arrives empty at `finish=length`. When off,
  the runner falls back to `reasoning_content` if content is empty, and flags
  the split.
- Preflight: waits for `GET /health` before scoring (a flipped model can take
  tens of seconds to load; without this every task scores 0).
- JSON strings are unescaped before grading, so line-based graders (task 4/5)
  see real newlines.

## Regression contract

- **qwen-class models must print 6/10** (matches the legacy harness baseline).
  Measured with the Rust runner: qwen3.5-2b-fresh **6/10**, granite-tiny **5/10**.
- A model that never produces parseable output scores 0 honestly -- the runner
  never fabricates a score.

## Outputs

- `scores.json` -- per-task score/max/finish/wall/grader note + total
- `transcript-<label>/task_*.txt` -- the raw model output per task

## License

MIT (same as the rest of the lab tools).