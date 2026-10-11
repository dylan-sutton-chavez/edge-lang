# Fuzzing

AFL++ fuzzing of the lexer, the parser and the VM through cargo-afl, on stable Rust. The [Runbook](https://edgepython.com/docs/edge-python/internals/runbook) explains what it covers and how CI runs it.

## Commands

Run them from the repo root. They need Linux or macOS and `cargo install cargo-afl`. On macOS, run `cargo afl system-config` once to raise the shared memory limits AFL needs.

| Command | Does |
|---|---|
| `make seeds` | Rebuilds `in/` from `tests/cases/vm.json` and `edge.dict` from `dict.txt` |
| `make fuzz` | Builds the target and runs one instance per core |
| `make fuzz-carry` | Minimizes the queues into `corpus/`, which the next campaign starts from |
| `make fuzz-status` | Shows a running campaign |
| `make fuzz-triage` | Counts the saved crashes by panic site |
| `make fuzz-replay CASE=out/m0/crashes/<id>` | Replays one input with a backtrace |
| `make fuzz-container` | Runs the campaign detached in Docker |
| `make fuzz-stop` | Stops the Docker campaign |

`make fuzz` and `make fuzz-container` take these variables.

| Variable | Default | Meaning |
|---|---|---|
| `JOBS` | one per core | AFL instances |
| `DURATION` | `0` | Seconds to run, `0` runs until stopped |
| `FRESH` | `0` | `1` wipes `out/` first |
| `TIMEOUT_MS` | `5000` | Time one input may take before it counts as a hang |
| `SEEDS` | `corpus`, or `in` before it holds inputs | Where a fresh campaign starts, `in` for a clean one |
| `SCHEDULES` | AFL's own | Power schedules handed to the instances in turn |
| `MUTATE` | none | Flags every instance takes, `-L 0` for MOpt |

## Findings

- Crashes and hangs land in `out/m0/`, `out/s1/` and onward, one folder per instance.
- One bug saves many inputs. `make fuzz-triage` groups them by the `file:line` they panic at.
- A resumed campaign archives older findings as `crashes.<date>/`, and triage reads those too.
- An arithmetic overflow panic comes from the overflow checks cargo-afl forces. The release VM runs without them.
- A hang is usually an input that ends but runs past `TIMEOUT_MS`.

## Resuming

`make fuzz` resumes an existing `out/`, even after a rebuild, and otherwise starts from `corpus/`, which CI keeps in its cache.

A Docker campaign keeps its findings in the `findings` volume, which survives `make fuzz-stop`. `docker compose down -v` in `fuzz/` deletes it.
