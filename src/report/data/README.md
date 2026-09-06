# Clanker Index v1 data

These files are embedded in the binary. They do not cause network requests or change stored language measurements.

- `linguist-language-types-v1.json`: the `type` and optional `group` fields from the user-supplied `languages.yml` (833 canonical names: 560 programming, 184 data, 71 markup, 18 prose). `source_sha256` identifies the original supplied file; no upstream commit is asserted.
- `clanker-language-tierlist-v1.json`: frozen explicit tier overrides from the accepted `clanker-index-tierlist.md`. JSON null means an explicit unjudged decision; a missing entry permits registry-group inheritance. The 39 same-tier grouped variants inherit, while differing child tiers remain overrides. Bend 2 and Psi retain their earlier project-lore assignments.
- `clanker-presentation.json`: terminal-only emoji and verdicts for all 560 programming entries plus the two lore languages. Child presentation remains independent of inherited tier semantics.

The accepted editorial table is the regression baseline for all 560 effective tiers. Fixed Rust/Smalltalk/Nix/OCaml/Zig/Go/C/C++/Python/Ruby/1C/Perl decisions remain intact. Semantic changes to this frozen policy, including classification applicability, require a new version. A wording or emoji edit does not.

Unknown measured names are not inferred from file extensions or aliases. They are reported as unknown type and excluded from grading. Markup/data/prose are ancillary even if they superficially resemble programming languages. Neither category is a Clanker Council `???` decision.
