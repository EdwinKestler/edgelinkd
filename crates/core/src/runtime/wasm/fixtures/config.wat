;; Stores the configuration passed to el_init and emits it for every message; el_close fails
;; with "bye" so tests can see that it ran.
(module
  (import "edgelink:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32)))
  (import "edgelink:node/v1" "fail" (func $fail (param i32 i32) (result i32)))
  (memory (export "memory") 1 1)
  (global $len (mut i32) (i32.const 0))
  (data (i32.const 4000) "bye")
  (func (export "el_abi_version") (result i32) i32.const 1)
  (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "el_init") (param $ptr i32) (param $len i32) (result i32)
    (if (i32.gt_u (local.get $len) (i32.const 1024)) (then (return (i32.const 1))))
    (memory.copy (i32.const 2048) (local.get $ptr) (local.get $len))
    (global.set $len (local.get $len))
    i32.const 0)
  (func (export "el_on_input") (param i32 i32) (result i32)
    (call $emit (i32.const 0) (i32.const 2048) (global.get $len)))
  (func (export "el_close")
    (drop (call $fail (i32.const 4000) (i32.const 3)))))
