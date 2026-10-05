;; Hostile: emits a 1 MiB buffer (over the 64 KiB per-emit cap) and then a negative length.
(module
  (import "edgelink:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1 1)
  (func (export "el_abi_version") (result i32) i32.const 1)
  (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "el_on_input") (param i32 i32) (result i32)
    (drop (call $emit (i32.const 0) (i32.const 0) (i32.const 1048576)))
    (call $emit (i32.const 0) (i32.const 0) (i32.const -16))))
