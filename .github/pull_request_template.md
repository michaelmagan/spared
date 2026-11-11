## What changed and why

## How it was tested

Commands you ran and what they showed.

## Checklist

- [ ] `cargo build --release -p spared` and `cargo test -p spared` pass
- [ ] If the CLI, templates or error kinds changed, `AGENTS.md` is updated
- [ ] If the sandbox, grants or outbound HTTP changed, an escape case is added or updated and `bench/escape-tests.sh` passes (it builds the lab binary on first use)
- [ ] Any number in prose names the file in `bench/results/` or the section of `docs/benchmarks.md` it came from
- [ ] No new dependency without a license check (`THIRD_PARTY.md`)
