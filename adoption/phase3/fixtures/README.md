# Phase 3 rollback fixtures

Both files use port `19889` and the compiled defaults for classes not shown. `strict.toml` rejects
an editor/admin body larger than 16 bytes. `editor-observe.toml` changes only that class to
`observe`, so the same request reaches its handler while health and every other class remain in
`enforce`.

Copy one file to `edgelinkd.toml` in a temporary EdgeLinkd home together with the sanitized Phase 0
flow/credential fixtures. Never point these deliberately small limits at a production home.
