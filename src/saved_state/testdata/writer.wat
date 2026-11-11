;; A WASI command that opens log.bin in its first preopened directory and
;; appends 10 records of 12 bytes: a counter (1 to 10, u32) and the
;; monotonic clock (u64, ns), sleeping 1 ms after each. The tests of
;; wasm_headless.rs hibernate it in the middle and check that the resumed
;; run continues the same file at the same offset, with a clock that never
;; goes back.
;;
;; writer.asyncify.wat is this module after Binaryen's Asyncify. To make it
;; again, from the m-browser directory:
;;   wasm-tools parse src/saved_state/testdata/writer.wat -o /tmp/writer.wasm
;;   wasm-opt --asyncify -O1 -g /tmp/writer.wasm -o /tmp/writer.asyncify.wasm
;;   wasm-tools print /tmp/writer.asyncify.wasm > src/saved_state/testdata/writer.asyncify.wat
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "clock_time_get" (func $clock_time_get (param i32 i64 i32) (result i32)))
  (import "wasi_snapshot_preview1" "poll_oneoff" (func $poll_oneoff (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 100) "log.bin")
  ;; A subscription at 304 (48 bytes): the monotonic clock (id 1, at +16),
  ;; relative, timeout 1,000,000 ns (at +24). Its event goes to 400.
  (data (i32.const 320) "\01")
  (data (i32.const 328) "\40\42\0f\00")
  (func $record (param $fd i32) (param $n i32)
    (i32.store (i32.const 204) (local.get $n))
    (if (call $clock_time_get (i32.const 1) (i64.const 1) (i32.const 208)) (then unreachable))
    (i32.store (i32.const 16) (i32.const 204))
    (i32.store (i32.const 20) (i32.const 12))
    (if (call $fd_write (local.get $fd) (i32.const 16) (i32.const 1) (i32.const 24)) (then unreachable)))
  (func $sleep
    (if (call $poll_oneoff (i32.const 304) (i32.const 400) (i32.const 1) (i32.const 24)) (then unreachable)))
  (func $_start (export "_start")
    (local $fd i32) (local $n i32)
    (if (call $path_open (i32.const 3) (i32.const 0) (i32.const 100) (i32.const 7) (i32.const 9)
          (i64.const 0x1fffffff) (i64.const 0) (i32.const 0) (i32.const 0))
      (then unreachable))
    (local.set $fd (i32.load (i32.const 0)))
    (loop $l
      (local.set $n (i32.add (local.get $n) (i32.const 1)))
      (call $record (local.get $fd) (local.get $n))
      (call $sleep)
      (br_if $l (i32.lt_u (local.get $n) (i32.const 10)))))
)
