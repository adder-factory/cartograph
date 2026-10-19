# Native storage distribution notices

This notice describes Cartograph `v3.0.1` and its native RocksDB backend.
Cartograph code remains MIT licensed. The executable now includes native
RocksDB and compression libraries; their upstream terms remain in force.

Each release archive contains exactly:

- `bin/cartograph` (`bin/cartograph.exe` on Windows)
- `LICENSE`
- `README.md`
- `ACKNOWLEDGEMENTS.md`
- `share/cartograph/NATIVE-STORAGE-NOTICE.md` (this document)
- `share/cartograph/THIRD-PARTY-STORAGE-LICENSES.txt`

The storage license bundle reproduces the pinned upstream texts for
`rust-rocksdb` (Apache-2.0), RocksDB 11.8.1 (Apache-2.0 selected), LevelDB
portions (BSD-3-Clause), LZ4 1.10.0 library (BSD-2-Clause), and Zstandard 1.5.7
library (BSD-3-Clause), plus lz4_flex 0.14.0 (MIT) for publication blocks.
Disabled Snappy/bzip2/zlib features are not part of this
storage build. Other Rust dependencies retain their declared licenses and
remain subject to the strict dependency license/advisory gate.

The Rust binding is pinned to commit
`50017c3de5bc24cf5d81fdc0fe6b0f23df700765` in Cargo.toml/Cargo.lock. Build from
those reviewed sources and preserve notices when distributing the executable.

Release audits require the exact file allowlist, no private build-root paths,
no separate database-server/container artifacts, no SQLX/PostgreSQL or SQLite
runtime dependencies, and the pinned native engine in the dependency tree.
Compile, archive smoke, checksum, provenance, and independent review evidence
are required before a new release is published. Historical ParadeDB release
notices remain part of their original archives and release records.

Upstream sources:

- [rust-rocksdb](https://github.com/zaidoon1/rust-rocksdb)
- [RocksDB](https://github.com/facebook/rocksdb)
- [LevelDB](https://github.com/google/leveldb)
- [LZ4](https://github.com/lz4/lz4)
- [lz4_flex](https://github.com/PSeitz/lz4_flex)
- [Zstandard](https://github.com/facebook/zstd)
