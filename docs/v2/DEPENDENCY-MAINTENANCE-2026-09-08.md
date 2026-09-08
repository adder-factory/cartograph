# Dependency maintenance review — 2026-09-08

The maintenance review found **no confirmed abandoned dependency requiring
removal** from the current development dependency graph. No package was replaced
or removed. This conclusion is a dated review of available evidence, not a
guarantee of future support. This audit did not publish a release.

## Scope and evidence

The locked, all-feature Cargo graph contains 308 registry packages and one local
Tree-sitter compatibility facade, outside the 12 workspace crates. Of the
registry packages, 86 are direct workspace dependencies. The inventory includes
build, development and target-specific transitive dependencies, including those
not compiled for Cartograph's release platforms.

The review checked:

- Fresh RustSec advisories and registry yank status, first with the four release
  targets and then with no target filtering. Both advisory checks passed with no
  ignored advisories. The database revision was
  `bf25f6575a93a35f30796c65c0ed91bee7fa19fd`.
- The 165 GitHub repositories referenced by registry packages, resolving moved
  repository names. None was archived, disabled or unavailable. Default-branch
  commits and current README notices were inspected alongside the published
  packages' maintenance badges and notices.
- Three GitLab source repositories: kqueue, kqueue-sys and Redox syscall,
  including the Redox project's move to its kernel repository. Their commit APIs
  showed recent activity. The project API responses did not expose an archive
  flag, so that flag is not claimed as verified for these repositories.
- Thirteen additional repositories supplying the managed runtime, Rust build
  tooling, coverage/advisory tools and the six GitHub Actions used by the
  workflows. These repositories were available and not archived or disabled.
- Reverse dependency paths for the TLS/certificate stack and parser bindings,
  to establish the effect of a possible replacement.

Cargo manifests declare 41 direct Arborium grammar pins, all at 2.18.2; the graph
also contains Arborium's JavaScript and sysroot packages transitively. This audit
corrects the earlier dependency report's count of 40 direct Arborium pins.

Repository activity, a missing release, a low version number or a small
contributor count cannot independently prove abandonment. An archived source,
an applicable maintenance advisory, or an explicit maintainer statement would
justify removal or a replacement investigation. Quiet projects and requests for
additional maintainers receive specific follow-up below.

## Decisions and watchlist

| Dependency | Evidence and decision |
| --- | --- |
| `ring` 0.17.14 | Retain. The broad unmaintained advisory was withdrawn. The remaining advisory applies to versions before 0.17. Its current upstream also has July 2026 commits. |
| `security-framework` 3.7.0 and `security-framework-sys` 2.17.0 | Retain and monitor. The published manifests say `looking-for-maintainer`, while upstream has August 2026 commits. These are transitive dependencies of native certificate loading and platform verification. Removing them would require a deliberate change to macOS certificate handling, not a package-name substitution. |
| `tree-sitter-luau` 1.2.0 | Monitor. The default-branch head is dated December 2024. The repository is available and unarchived; the inspected notices and maintainer-related issue search did not establish abandonment. Preserve Luau support rather than remove its grammar based solely on inactivity. |
| `tree-sitter-typescript` 0.23.2 | Monitor. The default-branch head is dated January 2025, with later repository pushes. No inspected maintainer notice establishes abandonment. Its age does not by itself justify replacing the tested TypeScript/TSX grammar. |
| `tree-sitter-astro-next` 0.1.1 | Monitor. It is a small repository with February 2026 commits. Its current README documents the supported grammar and accepts contributions; no abandonment notice was found. |
| WASI preview-1 bindings | Retain as target-specific transitive dependencies. The upstream says the frozen preview-1 standard's bindings are in maintenance mode. This is not a declaration that the project has no maintainer. These bindings are not compiled for Cartograph's native release targets. |
| `redox_syscall` 0.5.18 | Retain as a target-specific transitive dependency. The recorded source project points to the active Redox kernel repository. A repository move is not abandonment; this crate is not compiled for the native release targets. |
| Local Tree-sitter 0.26 facade | Retain under Cartograph ownership. This is a type-reexport adapter to the sole native 0.27 runtime, not a second old parser implementation. Remove it when published ABAP bindings accept 0.27 directly, as specified in the [vendor contract](../../vendor/README.md). |

Other quiet transitive utilities, including scopeguard, untrusted, subtle,
streaming-iterator and tinyvec_macros, have no applicable maintenance advisory or
abandonment notice in the inspected evidence. Their age remains a monitoring
signal. The SQLx README's abandoned-`dotenv` note concerns a different package;
`dotenv` is absent from this Cargo graph.

## Enforcement and validation

`deny.toml` now explicitly sets `unmaintained = "all"`, preserving the existing
cargo-deny behavior for both direct and transitive dependencies. It also sets
`yanked = "deny"`, strengthening the previous warning-only default. No advisory
ignore, dependency exclusion or weakened license rule was added.

The full configured cargo-deny gate passes: advisories, bans, licenses and
sources. A separate unfiltered advisory pass also covers the complete resolved
graph. The workspace dependency and release-workflow contracts pass. Since this
audit changes policy and documentation only, the Rust source and Cargo lockfile
from the previously validated architecture/retention package remain unchanged;
no database migration, binary installation, release or runtime replacement is
part of this maintenance review.

Recheck the watchlist and refresh RustSec before publication. If a package is
confirmed abandoned, evaluate its replacement through the actual dependency
path and preserve language support, certificate behavior, determinism and the
existing release gates.

## Primary sources

- [RustSec's withdrawn broad ring advisory](https://rustsec.org/advisories/RUSTSEC-2025-0007.html)
  and [the pre-0.17 advisory](https://rustsec.org/advisories/RUSTSEC-2025-0010.html).
- [macOS security-framework source](https://github.com/kornelski/rust-security-framework)
  and [discussion of an alternative binding](https://github.com/kornelski/rust-security-framework/issues/224).
- [Luau grammar](https://github.com/tree-sitter-grammars/tree-sitter-luau),
  [TypeScript grammar](https://github.com/tree-sitter/tree-sitter-typescript),
  and [Astro grammar](https://github.com/PRRPCHT/tree-sitter-astro-next).
- [WASI bindings and maintenance-mode explanation](https://github.com/bytecodealliance/wasi-rs).
- [kqueue](https://gitlab.com/rust-kqueue/rust-kqueue),
  [kqueue-sys](https://gitlab.com/rust-kqueue/rust-kqueue-sys), and
  [Redox kernel](https://gitlab.redox-os.org/redox-os/kernel).
- [cargo-deny advisory policy](https://embarkstudios.github.io/cargo-deny/checks/advisories/cfg.html)
  and [target-filtering semantics](https://embarkstudios.github.io/cargo-deny/checks/cfg.html).
