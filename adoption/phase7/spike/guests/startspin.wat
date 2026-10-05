;; Hostile: burns CPU in a start function, i.e. during instantiation.
(module
  (memory (export "memory") 1 1)
  (func $start (loop $forever (br $forever)))
  (start $start)
  (func (export "el_abi_version") (result i32) i32.const 1)
  (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "el_on_input") (param i32 i32) (result i32) i32.const 0))
