;; A command that adds up n * 3 + tick(n) for n = 0..9, calling the host's
;; env.tick in a nested function, so that a save in the middle has a call
;; stack to unwind. Without a save it ends with total = 4635.
;;
;; count.asyncify.wat is this module after Binaryen's Asyncify. To make it
;; again, from the m-browser directory:
;;   wasm-tools parse src/saved_state/testdata/count.wat -o /tmp/count.wasm
;;   wasm-opt --asyncify -O1 -g /tmp/count.wasm -o /tmp/count.asyncify.wasm
;;   wasm-tools print /tmp/count.asyncify.wasm > src/saved_state/testdata/count.asyncify.wat
(module
  (import "env" "tick" (func $tick (param i32) (result i32)))
  (memory (export "memory") 1)
  (global $total (mut i32) (i32.const 0))
  (func $inner (param $n i32) (result i32)
    (local $acc i32)
    (local.set $acc (i32.mul (local.get $n) (i32.const 3)))
    (local.set $acc (i32.add (local.get $acc) (call $tick (local.get $n))))
    (local.get $acc))
  (func $_start (export "_start")
    (local $i i32)
    (loop $l
      (global.set $total (i32.add (global.get $total) (call $inner (local.get $i))))
      (i32.store (i32.const 0) (global.get $total))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const 10)))))
  (func $get_total (export "total") (result i32) (global.get $total))
)
