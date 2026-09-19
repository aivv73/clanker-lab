# Bound refresh resource use

Paclang processes four analysis targets concurrently by default. Each target may download 1 GiB, extract 4 GiB, contain 250,000 files, and spend 15 minutes in analysis before entering the resource-limited state. Configuration and explicit refresh flags may raise these limits. Paclang retains the latest ten published inventory snapshots and profiles they reference; `paclang cache prune` removes unreferenced profiles and disposable downloads, while reports never prune automatically.
