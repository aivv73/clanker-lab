# Domain docs

Engineering skills read this repo's domain documentation before exploring the codebase.

## Before exploring, read these

- `CONTEXT.md` at the repo root.
- `docs/adr/` records that affect the area being changed.

If these files do not exist, proceed silently. The `/domain-modeling` skill creates them when project terms or decisions need to be recorded.

## File structure

This repo uses a single-context layout:

```text
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0001-example-decision.md
│   └── 0002-another-decision.md
└── src/
```

## Use the glossary's vocabulary

When output names a domain concept in an issue title, proposal, hypothesis, or test, use the term defined in `CONTEXT.md`. Do not replace it with a synonym that the glossary rejects.

If the concept is missing, reconsider whether it belongs to the project. If it does, record the gap for `/domain-modeling`.

## Flag ADR conflicts

Explicitly identify output that contradicts an existing ADR instead of silently overriding the decision.
