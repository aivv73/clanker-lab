# Working on clanker-lab

## Agent skills

### Issue tracker

GitHub Issues: aivv73/clanker-lab. See `docs/agents/issue-tracker.md`.

### Triage labels

Five standard triage labels are used. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: the glossary and ADRs are in `paclang/`.
See `docs/agents/domain.md`.

These instructions apply to the entire monorepo. If an `AGENTS.md` closer to the file being edited is added, it refines the rules for that package.

## Project structure

- `clanker-lab` is a monorepo of personal experiments. Its only package currently is `paclang/`, a Rust CLI for analyzing the programming languages used in the source code of installed packages on ALPM-based systems.
- Each experiment keeps its code, dependencies, tests, and documentation in its own directory. Do not introduce shared infrastructure or dependencies between experiments unless the task requires them.
- There is no shared Cargo workspace yet. Run Cargo commands from `paclang/`.
- When adding a package, update the root `README.md`; keep detailed usage instructions in the package's own README.

## Searching and reading

- Use local files as the source of information about this project. Do not search the workspace for evidence about unrelated external questions.
- For known names, paths, strings, and regular expressions, use `zvec_grep_rg` when available; otherwise, use `rg`.
- For semantic discovery, relationships, and comparisons across files, use `zvec_grep_search` when available, then narrow the results with focused searches. If the tool is unavailable, use `rg` and targeted file reads.
- Always pass the absolute workspace path as `root` in zvec-grep calls. Check `freshness` and `background_refresh` in the results; sufficient results marked `served_from_current_index` can be used without waiting for a refresh.
- If the task is only to determine whether conceptually related material exists and no exact search anchor is available, make one focused semantic query. If the results are irrelevant, report that the index did not establish the answer.
- Treat a sufficient search snippet as already-read evidence. Open the file only for missing details; avoid broad reads before searching.
- Do not create, rebuild, or delete a persistent search index without an explicit user request or authorization.

## Changes to paclang

Before making changes, read `paclang/README.md`, the glossary in `paclang/CONTEXT.md`, and the relevant decisions in `paclang/docs/adr/`. Additional domain instructions are in `paclang/docs/agents/domain.md`. Paths in package documentation are relative to `paclang/` unless explicitly stated otherwise.

- Use the terms from `CONTEXT.md`. The languages used in package source code are not equivalent to installed compilers or interpreters.
- Preserve the distinctions between exact analysis of the packaged revision, approximate upstream data, and stale profiles. Do not turn missing data into a zero result.
- Preserve data provenance, explicit limitations, and coverage metrics in reports. Subjective assessments must not replace measured facts.
- Keep access to installed packages read-only. Do not execute PKGBUILD files, downloaded source code, or hooks for analysis.
- Account for snapshot integrity, operation cancellation, resource limits, and validation of downloaded source code. When changing behavior, consult the relevant ADRs and explicitly identify any conflicts.
- Update documentation when changing the CLI or behavior. Validate report format changes against the tests and fixtures in `paclang/tests/`.

## Verification and reporting

For Rust code changes, run the following from `paclang/`:

```sh
cargo fmt --check
cargo check
cargo test
```

Add or update tests when behavior changes. Prefer reproducible local fixtures over external networks and the state of the installed system. For documentation-only changes, checking the accuracy of commands, paths, and links is sufficient.

Do not edit unrelated files or undo the user's changes. In the final response, briefly describe the result, the checks performed, and any remaining limitations. If a check could not be run, say so explicitly.
