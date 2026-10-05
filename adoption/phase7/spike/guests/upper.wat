;; Well-behaved ABI v1 guest: upper-cases ASCII input and emits it on port 0.
(module
  (import "edgelink:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1 1)
  (func (export "el_abi_version") (result i32) i32.const 1)
  (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "el_on_input") (param $ptr i32) (param $len i32) (result i32)
    (local $i i32) (local $c i32)
    (block $done
      (loop $next
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $c (i32.load8_u (i32.add (local.get $ptr) (local.get $i))))
        (if (i32.and (i32.ge_u (local.get $c) (i32.const 97)) (i32.le_u (local.get $c) (i32.const 122)))
          (then (i32.store8 (i32.add (local.get $ptr) (local.get $i)) (i32.sub (local.get $c) (i32.const 32)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $next)))
    (call $emit (i32.const 0) (local.get $ptr) (local.get $len))))
