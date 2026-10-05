;; Hostile: grows linear memory until the host refuses.
(module
  (memory (export "memory") 1)
  (func (export "el_abi_version") (result i32) i32.const 1)
  (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "el_on_input") (param i32 i32) (result i32)
    (loop $more
      (br_if $more (i32.ne (memory.grow (i32.const 1)) (i32.const -1))))
    i32.const 0))
