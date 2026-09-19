# Use report subcommands

Paclang exposes mutually exclusive reports as subcommands rather than flags. Plain `paclang` aliases `paclang count`; the initial commands are `count`, `source-bytes`, `installed-size-estimate`, `packages <language>`, `riir`, `rewrite-candidates`, `clanker-index`, and `refresh`. Common flags select package populations and output behavior. This grammar rejects ambiguous combinations such as requesting count, source bytes, and RIIR through competing flags, and it gives scripts stable command names.
