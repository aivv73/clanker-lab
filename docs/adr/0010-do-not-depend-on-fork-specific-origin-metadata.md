# Do not depend on fork-specific origin metadata

Paclang does not link to libalpm for package-origin metadata. `alpm_pkg_get_installed_db` and `%INSTALLED_DB%` are CachyOS extensions rather than upstream libalpm contracts, and an absent value cannot distinguish a foreign package from an older local-database entry. Paclang keeps package origin unknown instead of adding a distribution-specific runtime dependency or guessing from current sync repositories. The evidence and compatibility analysis is in [the issue #9 research note](../research/issue-9-libalpm-package-origin.md).
