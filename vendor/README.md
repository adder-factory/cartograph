# ABAP grammar compatibility with Tree-sitter 0.27

Cartograph uses the published Tree-sitter **0.27.0** native runtime. The current
`tree-sitter-abap-sqry` 32.0.1 and `sqry-tree-sitter-support` 32.0.1 releases still
request 0.26. Both bindings need only `Language` and the two language ABI constants.

`tree-sitter-026-compat` satisfies that dependency with direct reexports of the
exact 0.27 types. It has no native library, build script, parser, FFI, or conversion
code. The grammar and its validation support remain unchanged registry packages.
The workspace dependency gate verifies the sole native Tree-sitter runtime and
this facade's ownership and lack of a native build. Every grammar still passes
Cartograph's extraction corpus and publication/determinism gates.

The Cargo lockfile therefore contains two package versions named `tree-sitter`;
only the registry 0.27.0 package owns the native `links = "tree-sitter"` library.
The narrowly scoped cargo-deny entry records that distinction. The facade and
its manifest are included in the extractor fingerprint so edits invalidate cached
facts. Remove the patch and its matching cargo-deny entry once upstream's published
ABAP bindings accept 0.27 directly.
