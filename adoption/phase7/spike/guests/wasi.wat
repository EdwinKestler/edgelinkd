;; Hostile: asks for an ambient WASI capability.
(module
  (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1 1)
  (func (export "el_abi_version") (result i32) i32.const 1)
  (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "el_on_input") (param i32 i32) (result i32) i32.const 0))
