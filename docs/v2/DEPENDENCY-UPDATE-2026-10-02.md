# Dependency update — 2026-10-02

This audit records the move to Rust 1.99.0, which the
[2026-10-01 audit](DEPENDENCY-UPDATE-2026-10-01.md) deferred until the official
build image was published. Publication requires the separate local, live,
Sonar, reviewer, and remote artifact gates.

## Toolchain

| Component | Previous | Selected |
| --- | --- | --- |
| Rust compiler (`rust-toolchain.toml`) | 1.98.1 | 1.99.0 |
| Workspace and Tree-sitter facade `rust-version` | 1.98.1 | 1.99.0 |
| cargo-deny toolchain (`v2-rust.yml`) | 1.98.1 | 1.99.0 |

Every required gate still uses this exact stable compiler. The Linux build
helper still installs the pinned compiler into the build image and checks
`rustc --version` before building. The parse-cache fingerprint includes the
pinned toolchain, so existing parse caches are rebuilt once. The generation
contract stays at **V18** and the schema is unchanged. The only lockfile
change is the new `signal-hook` dependency described below; ParadeDB is
unchanged.

## Build images

The official `rust:1.99.0-trixie` image is now published. Both workflows pin it
by platform digest, the same way the 1.98.1 image was pinned:

| Image | Platform | Selected immutable digest |
| --- | --- | --- |
| rust:1.99.0-trixie (index) | — | `sha256:7f3a6cc67eb9622bfb208585cd35d5c4294ad73e21b01d6269340ec914318323` |
| rust:1.99.0-trixie | amd64 | `sha256:9d5e02aa6c7e9c112ed7a4c438900b41f519a792959cd5445c25de42bfb8388b` |
| rust:1.99.0-trixie | arm64 | `sha256:853d02b24a2315b5f276c5126aa7e690b6cfe6d146b0278fa654969e119e3072` |
| debian:13-slim | amd64 | `sha256:7792b1f7702a86946cd518db72b6a407302c3e9bc1635634368b878189e8221c` (unchanged) |
| debian:13-slim | arm64 | `sha256:da496358bd6934d2bd6a563a33176a2e50eff5490c54b4ac6fb051b69fef4071` (unchanged) |

The digests come from the Docker Hub registry API. Each manifest's SHA-256
matches the digest the registry reports. Each platform image config names the
expected OS and architecture, and the Rust images set `RUST_VERSION=1.99.0`.
The Linux build helper and the release workflow contract test now require the
1.99.0 image.

## Changes the new toolchain requires

Rust 1.99.0 and its Clippy report three new kinds of finding in the workspace.
Each is fixed in the code, without suppressions:

- `AtomicU64::fetch_update` is deprecated in favor of `try_update`. The
  saturating counters (agent observation counts, prepare-transaction progress,
  and the supervisor's heartbeat count) always produce a new value, so they
  call the infallible `update`. Their behavior is unchanged.
- The new pedantic lint `clippy::assert_is_empty` flags `assert!` checks of
  `is_empty()` because they hide the value when they fail. The 90 flagged test
  assertions now use `assert_eq!` or `assert_ne!` against an empty literal, so
  a failure prints the unexpected contents. A typed literal such as
  `[] as [String; 0]` is used where the element type is otherwise ambiguous.
- `clippy::single_element_loop` now reports a loop over a one-element array
  literal. The MCP setup check names that array `REQUIRED_LLM_TIERS`; the
  embedding tier remains the only required tier.

The other 1.99 lint changes do not fire in this workspace: deprecated legacy
integer modules, `semicolon_in_expressions_from_non_local_macros` for macros
from other crates, unused `#[path]` attributes on inline modules, and
`unreachable_cfg_select_predicates`.

## New dependency

| Crate | Version | Scope |
| --- | --- | --- |
| `signal-hook` | 0.4.4 (latest) | `cartograph-cli`, Unix targets only, default features off |

The workspace forbids `unsafe` code. Restoring a signal's default disposition
and re-raising it needs `sigaction` and `raise`, so the CLI uses
`signal_hook::low_level::emulate_default_handler` for that one step: a second
interrupt, an interrupt before or after a cancellable index request, or one
the process inherited as ignored now ends the process by that signal, the way
the default disposition would. Windows keeps exiting with
`STATUS_CONTROL_C_EXIT` and needs no new crate. The crate shares the
`signal-hook-registry` and `libc` versions already in the lockfile (through
Tokio), and `cargo deny` accepts its Apache-2.0/MIT license. The 0.4 line
changed only the `low_level::pipe` API, which Cartograph does not use.

Sources: [Rust releases](https://blog.rust-lang.org/releases/) and the
[official Rust images](https://hub.docker.com/_/rust).
