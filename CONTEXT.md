# Paclang

Paclang analyzes the programming languages behind packages installed on an ALPM-based system. Its measured reports separate package facts from deliberately opinionated joke reports.

## Language

**Installed package**:
A binary package recorded in the local ALPM database. The default population includes both explicit packages and dependency packages.
_Avoid_: Program, application

**ALPM system**:
An Arch-derived installation whose local package state is managed by ALPM, including installations that use derivative or third-party repositories.
_Avoid_: Arch/AUR system

**Package origin**:
Package origin is the repository or foreign-package status from which an installed package entered the ALPM system. It is unknown when installed metadata does not expose that fact, and a match in current repository metadata is not a package origin.
_Avoid_: Upstream, source repository

**Source provider**:
An adapter that uses package-origin metadata to find package recipes and source evidence. Unsupported origins remain visible as unresolved rather than being treated as Arch or AUR packages.
_Avoid_: Repository, resolver heuristic

**Provider attempt**:
One source provider's recorded evidence or failure while resolving an installed package. Inspection shows every attempt rather than only the chosen result.
_Avoid_: Analysis failure, retry

**Explicit package**:
An installed package whose ALPM install reason marks it as explicitly installed rather than installed as a dependency.
_Avoid_: User package, manually installed package

**Package count**:
A count in which each installed package name contributes one unit, including each installed member of a split package.
_Avoid_: Project count, repository count

**Selected population**:
The installed packages included by explicit-package, dependency-package, and package-origin filters. Every percentage uses the selected population or identifies a different denominator.
_Avoid_: Scope, package set

**Analysis target**:
A canonical source location, revision, and optional subdirectory analyzed once by Linguist. Several installed packages may share one analysis target.
_Avoid_: Package, homepage, upstream

**Primary source**:
The source tree that represents the packaged project. Recipe patches, icons, generated files, and unrelated archives are evidence or build inputs rather than parts of this tree.
_Avoid_: All recipe sources, homepage

**Packaged source revision**:
The source revision or release used to build an installed package.
_Avoid_: Latest source, upstream HEAD

**Source-language profile**:
Linguist's detected language-byte distribution for one analysis target.
_Avoid_: Package languages, installed languages

**Source-byte report**:
A report that sums each unique analysis target once within the selected population, even when several selected installed packages share that target.
_Avoid_: Package source size, installed source bytes

**Primary language**:
The language with the largest byte count in an analysis target's source-language profile.
_Avoid_: Implementation language

**Installed-size estimate**:
An installed package's byte size distributed according to its analysis target's source-language profile. It does not measure the languages of files installed on disk.
_Avoid_: Installed language bytes, disk usage by language

**Analysis state**:
The reason an installed package has or lacks a source-language profile. States distinguish unresolved source, unavailable revision, no detected language, analysis failure, stale analysis, and current analysis.
_Avoid_: Unknown

**Analysis database**:
The persistent inventory snapshots, source evidence, provider attempts, and source-language profiles stored under the user's data directory. It does not retain source trees.
_Avoid_: Analysis cache, source cache

**Refresh**:
An explicit networked operation that resolves sources, acquires packaged revisions, runs Linguist, and updates the analysis database.
_Avoid_: Report, scan

**Inventory snapshot**:
The selected installed-package inventory captured at the start of a refresh. Reports use one published snapshot and never combine results from different inventories.
_Avoid_: Analysis cache, live inventory

**Metadata-only refresh**:
An explicit operation that publishes installed-package inventory metadata without resolving, acquiring, or analyzing primary sources.
_Avoid_: Dry run, offline refresh

**Report model**:
The typed counts, percentages, coverage, states, and provenance shared by every output format. A formatter may change layout and decoration but not the reported facts.
_Avoid_: Output, rendered report

**Coverage**:
The selected installed-package population divided by analysis state, including packages with current, stale, missing, or failed analysis.
_Avoid_: Success rate, resolved percentage

**Coverage threshold**:
A caller-selected minimum analyzed share required for a report to exit successfully. It does not change the report's counts or hide missing analysis.
_Avoid_: Coverage target, resolution goal

**Stale analysis**:
A cached source-language profile that does not match the installed package version or current analysis inputs. Reports include it by default but disclose its contribution separately.
_Avoid_: Current analysis, unknown

**User override**:
A local correction that supplies primary-source evidence for a package or package base. Reports identify results derived from an override.
_Avoid_: Cache entry, provider result

**Evidence grade**:
The strength of an analysis target mapping: exact, reconstructed, fallback, or override. It replaces a misleading resolved-or-unresolved boolean.
_Avoid_: Confidence score, resolved

**Ambiguous mapping**:
Two or more equally supported analysis targets for one installed package. Paclang preserves all candidates and selects none.
_Avoid_: Unknown, provider failure

**Linguist adapter**:
A configured native executable or pinned OCI image that runs GitHub Linguist. Paclang does not download or install an adapter automatically.
_Avoid_: Linguist runtime, language detector

**Materialized archive**:
A verified source archive placed into a temporary Git repository so Linguist can analyze its tree. Inspection distinguishes it from an upstream Git checkout.
_Avoid_: Source repository, clone

**Resource-limited analysis**:
An analysis stopped by a configured download, extraction, file-count, duration, or concurrency limit. It is an analysis state, not a refresh-wide failure.
_Avoid_: Unknown, analysis failure

**Other**:
A presentation bucket that combines language rows hidden by a display limit. It is not a language in the report model.
_Avoid_: Unknown, uncategorized language

**Terminal format**:
The human-facing report format with aligned tables, progress bars, color, and experimental jokes.
_Avoid_: Text format, pretty format

**TOON format**:
The compact, structured report format intended for coding agents and non-interactive command use.
_Avoid_: Terminal format, TOON mode

**Report schema version**:
The semantic version of the structured success and error model, independent of the paclang executable version.
_Avoid_: Paclang version, cache version

**TOON specification version**:
The working-draft TOON version targeted by paclang's serializer and conformance fixtures.
_Avoid_: Report schema version, paclang version

**Rust package**:
An installed package whose analysis target has Rust as its primary language.
_Avoid_: Package containing Rust

**RIIR progress**:
An experimental report that divides selected installed packages into Rust, Not Rust, and Unknown using one shared denominator.
_Avoid_: Rust adoption, migration progress

**Clanker Approved Language Index**:
An experimental system score that weights detected languages with a fixed 0-to-100 satirical tier rubric. Unranked languages appear as `???` and are excluded from the score.
_Avoid_: Language quality, rewrite priority

**Aspirational language**:
A language listed in the Clanker Index rubric that Linguist cannot currently detect and therefore cannot affect the system score.
_Avoid_: Detected language, unranked language

**Rewrite candidate**:
A non-Rust installed package ranked by installed size for satirical output. The ranking does not claim that a rewrite is feasible or valuable.
_Avoid_: Migration recommendation

**Measured report**:
Output derived from package metadata and source-language analysis, with unresolved and stale data disclosed.
_Avoid_: Accurate mode, serious mode

**Experimental report**:
An explicitly selected, opinionated interpretation such as RIIR progress or the Clanker Approved Language Index.
_Avoid_: Analysis, measured report
