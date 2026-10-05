;; Echo the input bytes on port 0 (EVE-in, EVE-out).
(module
  (import "edgelink:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1 1)
  (func (export "el_abi_version") (result i32) i32.const 1)
  (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "el_on_input") (param $ptr i32) (param $len i32) (result i32)
    (call $emit (i32.const 0) (local.get $ptr) (local.get $len))))
