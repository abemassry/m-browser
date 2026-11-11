;; A WASI command that prints "tick 1", "tick 2", ... "tick 3600" to its
;; standard output, one line a second (it sleeps with poll_oneoff), then
;; exits. Open it in m-browser as a page (file:///.../ticker.wasm) to see a
;; core module's text page, and that the ticks stop while its tab is in the
;; background and go on from the same number when it comes back. The test
;; a_ticker_prints_to_its_page in wasm_headless.rs runs it.
;;
;; To build it, from the m-browser directory:
;;   wasm-tools parse src/saved_state/testdata/ticker.wat -o ticker.wasm
(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "poll_oneoff" (func $poll_oneoff (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; A subscription at 304 (48 bytes): the monotonic clock (id 1, at +16),
  ;; relative, timeout 1,000,000,000 ns (at +24). Its event goes to 400.
  (data (i32.const 320) "\01")
  (data (i32.const 328) "\00\ca\9a\3b")
  ;; Print "tick <n>\n": the line is built backwards, ending at 240.
  (func $print_tick (param $n i32)
    (local $p i32)
    (local.set $p (i32.const 239))
    (i32.store8 (local.get $p) (i32.const 10))
    (loop $digits
      (local.set $p (i32.sub (local.get $p) (i32.const 1)))
      (i32.store8 (local.get $p) (i32.add (i32.const 48) (i32.rem_u (local.get $n) (i32.const 10))))
      (local.set $n (i32.div_u (local.get $n) (i32.const 10)))
      (br_if $digits (local.get $n)))
    (local.set $p (i32.sub (local.get $p) (i32.const 5)))
    (i32.store (local.get $p) (i32.const 0x6b636974))
    (i32.store8 (i32.add (local.get $p) (i32.const 4)) (i32.const 32))
    ;; One iovec at 16 (address, length); the count written goes to 24.
    (i32.store (i32.const 16) (local.get $p))
    (i32.store (i32.const 20) (i32.sub (i32.const 240) (local.get $p)))
    (if (call $fd_write (i32.const 1) (i32.const 16) (i32.const 1) (i32.const 24)) (then unreachable)))
  (func (export "_start")
    (local $n i32)
    (loop $again
      (local.set $n (i32.add (local.get $n) (i32.const 1)))
      (call $print_tick (local.get $n))
      (if (call $poll_oneoff (i32.const 304) (i32.const 400) (i32.const 1) (i32.const 24)) (then unreachable))
      (br_if $again (i32.lt_u (local.get $n) (i32.const 3600)))))
)
