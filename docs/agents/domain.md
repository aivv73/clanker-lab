# Domain docs

All paths below are relative to the clanker-lab root.

## Structure

There is currently one domain context: paclang.

- paclang/CONTEXT.md — glossary.
- paclang/docs/adr/ — architecture decisions.
- paclang/docs/agents/domain.md — additional context rules.

## Before exploring paclang code

Read paclang/README.md, paclang/CONTEXT.md,
paclang/docs/agents/domain.md, and the ADRs relevant to the task.

Use glossary terms in issues, proposals, and tests.
If a required concept is missing, flag the gap for /domain-modeling.
Explicitly identify conflicts with existing ADRs.

For new experiments, keep domain documentation inside their own directories
as instructed by the root AGENTS.md. Do not create missing documentation
in advance: /domain-modeling adds it as needed.
