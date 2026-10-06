# Cartograph v2 and ParadeDB distribution boundary

[Documentation home](https://github.com/adder-factory/cartograph/blob/main/docs/README.md) ·
[Project overview](https://github.com/adder-factory/cartograph/blob/main/README.md) ·
[Storage and operations](https://github.com/adder-factory/cartograph/blob/main/docs/STORAGE-BACKENDS.md) ·
[Release evidence](https://github.com/adder-factory/cartograph/blob/main/docs/v2/benchmarks/README.md)

Status: release policy for the local Cartograph v2 product. This records the
technical distribution boundary; it is not a substitute for legal advice.

This file is maintained as `docs/v2/LICENSING.md` and ships in every release
archive as `share/cartograph/PARADEDB-NOTICE.md`.

## What Cartograph releases distribute

The Cartograph GitHub release contains only the MIT-licensed Cartograph Rust
executable, Cartograph notices/documentation, and checksums/provenance for that
executable. It must not contain a ParadeDB image, `pg_search` binary, PostgreSQL
server, extension package, copied ParadeDB source, or a combined installer.

Each native archive contains exactly these files:

- `bin/cartograph` (`bin/cartograph.exe` on Windows)
- `LICENSE`
- `README.md`
- `ACKNOWLEDGEMENTS.md`
- `share/cartograph/PARADEDB-NOTICE.md` (this notice)

`cartograph db start` asks the user's local Docker daemon to pull the pinned
upstream ParadeDB image by manifest digest. The image remains a separately
installed and separately executed PostgreSQL service. External PostgreSQL is
also supported when the user has installed compatible `pg_search` and
`pgvector` extensions themselves.

The release audit must inspect every archive and fail if it contains a
PostgreSQL/ParadeDB executable, shared library, package, container layer, or
image archive. Changing this boundary requires a new legal review before
publication.

## Upstream license and supported use

ParadeDB states that its repository is available under AGPL-3.0 and commercial
licensing. Cartograph does not change, remove, or replace those terms. The
upstream project, license, and deployment links are carried in this notice
(see "Upstream references" below). The managed-database diagnostics report
only whether the container matches the supported digest-pinned image
(`image_matches`); they do not print license or upstream text.

ParadeDB's deployment documentation also warns that Community lacks WAL
support and discourages using it for a paying production application because a
crash or restart can require reindexing and downtime. Cartograph therefore
treats Community BM25 data as rebuildable derived local data:

- Cartograph v2 supports local developer/agent use with the pinned Community
  image;
- relational source-of-truth rows are backed up separately from rebuildable
  BM25/vector indexes;
- startup detects an unusable derived index and provides a bounded rebuild;
- shared, hosted, replicated, or customer-facing production deployments are
  outside the Community support claim and require an explicit durability and
  ParadeDB commercial/Enterprise licensing decision.

## Release review checklist

Before each stable release:

1. Confirm the pinned image digest and extension version still match the live
   capability suite on every supported database architecture.
2. Confirm native Cartograph archives contain no ParadeDB/PostgreSQL artifact.
3. Run the dependency license/advisory audit for the Rust executable.
4. Keep the upstream license and deployment links in the public notices.
5. Record a new review if Cartograph begins redistributing an image/extension,
   operating a hosted service, or offering a combined commercial product.

## Upstream references

- ParadeDB repository and license: <https://github.com/paradedb/paradedb>
- ParadeDB deployment documentation: <https://docs.paradedb.com/deploy/overview>
