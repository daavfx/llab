# llab + gguf-doctor

Two zero-dependency Rust tools for running a local llama.cpp zoo from one box.

## llab -- the live lab

A single std-only Rust binary (no crates) that serves one dashboard on
`http://127.0.0.1:7575` and owns the lifecycle of local `llama-server`
instances:

- **Fleet control** -- spawn/stop/switch engines per model from a registry
  JSON; every spawn is BelowNormal + detached; RAM guard before spawn;
  engine pins enforced (per-model allowlists).
- **Real metrics** -- CPU via PDH (`Processor(_Total)\% Processor Time`,
  Task-Manager-identical), memory via `GlobalMemoryStatusEx`, both kernel32
  FFI. No WMI polling, no dependencies.
- **Bench runner** -- drives `llama-bench -o jsonl`, parses rows, stamps every
  row with gguf + exe fingerprints, appends to an NDJSON store.
- **Chat proxy** -- humans and agents hit the same `/api/chat`; every session
  is a logged row (deterministic: temperature 0 + seed).
- **DSpark Overwatch** -- reads the fork's `Local\daavfx_dspark_<pid>` shared
  memory (see the fork repo's WIRE.md) and shows tap seq / captures / norms
  live in a TAPS tab. Flip any model with the `taps` checkbox to enable.
- **Quality axis** -- frozen-corpus perplexity A/B rows in the same store
  format (`results\ppl_rows.ndjson`).

UI: `src/dash.html`, embedded via `include_str!` -- carbon-platinum theme,
tabs (Ctrl+1..6), live polling, nothing pagescrolls.

### Run

```
cargo build --release
target\release\llab.exe            # LLAB_ROOT / LLAB_PORT env to relocate
```

`LLAB_ROOT` must contain:

```
GGUF\llm-collection\tools\gguf-play\registry.json   # model registry (authority for lab ops)
results\                                            # NDJSON stores are created here
doctor-sweep-*.txt                                  # latest gguf-doctor sweep (optional)
```

Endpoints: `/api/metrics /api/doctor /api/models /api/servers /api/taps
/api/rows?store= /api/engine/{flip,stop,state} /api/bench/{run,state}
/api/chat`.

## gguf-doctor -- header forensics

Scans GGUF files read-only and reports: version, tensor count, kv pairs,
arch/name, ctx, block count, file type, tokenizer model + pre-tokenizer, BOS/EOS,
chat template presence, plus checks: unknown pre-tokenizers, hard-seeded think
templates (budget-fill risk), missing eos, tiny ctx. One line per file, exit
code reflects worst verdict. Use it before serving any new download.

```
gguf-doctor.exe model.gguf        # single file
gguf-doctor.exe C:\zoo\*.gguf     # sweep
```

## License

MIT.
