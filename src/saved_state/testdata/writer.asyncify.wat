;; Generated from writer.wat by Binaryen's Asyncify: see that file. Do not edit.
(module
  (type (;0;) (func))
  (type (;1;) (func (param i32 i32 i32 i32) (result i32)))
  (type (;2;) (func (param i32)))
  (type (;3;) (func (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (type (;4;) (func (param i32 i64 i32) (result i32)))
  (type (;5;) (func (param i32 i32)))
  (type (;6;) (func (result i32)))
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (;0;) (type 3)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (;1;) (type 1)))
  (import "wasi_snapshot_preview1" "clock_time_get" (func $clock_time_get (;2;) (type 4)))
  (import "wasi_snapshot_preview1" "poll_oneoff" (func $poll_oneoff (;3;) (type 1)))
  (memory (;0;) 1)
  (global (;0;) (mut i32) i32.const 0)
  (global (;1;) (mut i32) i32.const 0)
  (export "memory" (memory 0))
  (export "_start" (func $_start))
  (export "asyncify_start_unwind" (func 7))
  (export "asyncify_stop_unwind" (func 8))
  (export "asyncify_start_rewind" (func 9))
  (export "asyncify_stop_rewind" (func 8))
  (export "asyncify_get_state" (func 10))
  (func $record (;4;) (type 5) (param i32 i32)
    (local i32 i32)
    global.get 0
    i32.const 2
    i32.eq
    if ;; label = @1
      global.get 1
      global.get 1
      i32.load
      i32.const 8
      i32.sub
      i32.store
      global.get 1
      i32.load
      local.tee 1
      i32.load
      local.set 0
      local.get 1
      i32.load offset=4
      local.set 1
    end
    block (result i32) ;; label = @1
      global.get 0
      i32.const 2
      i32.eq
      if ;; label = @2
        global.get 1
        global.get 1
        i32.load
        i32.const 4
        i32.sub
        i32.store
        global.get 1
        i32.load
        i32.load
        local.set 3
      end
      global.get 0
      i32.eqz
      if ;; label = @2
        i32.const 204
        local.get 1
        i32.store
      end
      global.get 0
      i32.eqz
      local.get 3
      i32.eqz
      i32.or
      if ;; label = @2
        i32.const 1
        i64.const 1
        i32.const 208
        call $clock_time_get
        local.set 2
        i32.const 0
        global.get 0
        i32.const 1
        i32.eq
        br_if 1 (;@1;)
        drop
        local.get 2
        local.set 1
      end
      global.get 0
      i32.eqz
      if ;; label = @2
        local.get 1
        if ;; label = @3
          unreachable
        end
        i32.const 16
        i32.const 204
        i32.store
        i32.const 20
        i32.const 12
        i32.store
      end
      global.get 0
      i32.eqz
      local.get 3
      i32.const 1
      i32.eq
      i32.or
      if ;; label = @2
        local.get 0
        i32.const 16
        i32.const 1
        i32.const 24
        call $fd_write
        local.set 2
        i32.const 1
        global.get 0
        i32.const 1
        i32.eq
        br_if 1 (;@1;)
        drop
        local.get 2
        local.set 0
      end
      global.get 0
      i32.const 1
      local.get 0
      select
      i32.eqz
      if ;; label = @2
        unreachable
      end
      return
    end
    local.set 2
    global.get 1
    i32.load
    local.get 2
    i32.store
    global.get 1
    global.get 1
    i32.load
    i32.const 4
    i32.add
    i32.store
    global.get 1
    i32.load
    local.tee 2
    local.get 0
    i32.store
    local.get 2
    local.get 1
    i32.store offset=4
    global.get 1
    global.get 1
    i32.load
    i32.const 8
    i32.add
    i32.store
  )
  (func $sleep (;5;) (type 0)
    (local i32 i32)
    global.get 0
    i32.const 2
    i32.eq
    if ;; label = @1
      global.get 1
      global.get 1
      i32.load
      i32.const 4
      i32.sub
      i32.store
      global.get 1
      i32.load
      i32.load
      local.set 0
    end
    block (result i32) ;; label = @1
      global.get 0
      i32.eqz
      global.get 0
      i32.const 2
      i32.eq
      if (result i32) ;; label = @2
        global.get 1
        global.get 1
        i32.load
        i32.const 4
        i32.sub
        i32.store
        global.get 1
        i32.load
        i32.load
      else
        local.get 1
      end
      i32.eqz
      i32.or
      if ;; label = @2
        i32.const 304
        i32.const 400
        i32.const 1
        i32.const 24
        call $poll_oneoff
        local.set 1
        i32.const 0
        global.get 0
        i32.const 1
        i32.eq
        br_if 1 (;@1;)
        drop
        local.get 1
        local.set 0
      end
      global.get 0
      i32.const 1
      local.get 0
      select
      i32.eqz
      if ;; label = @2
        unreachable
      end
      return
    end
    local.set 1
    global.get 1
    i32.load
    local.get 1
    i32.store
    global.get 1
    global.get 1
    i32.load
    i32.const 4
    i32.add
    i32.store
    global.get 1
    i32.load
    local.get 0
    i32.store
    global.get 1
    global.get 1
    i32.load
    i32.const 4
    i32.add
    i32.store
  )
  (func $_start (;6;) (type 0)
    (local i32 i32 i32 i32)
    global.get 0
    i32.const 2
    i32.eq
    if ;; label = @1
      global.get 1
      global.get 1
      i32.load
      i32.const 8
      i32.sub
      i32.store
      global.get 1
      i32.load
      local.tee 0
      i32.load
      local.set 1
      local.get 0
      i32.load offset=4
      local.set 2
    end
    block (result i32) ;; label = @1
      global.get 0
      i32.const 2
      i32.eq
      if ;; label = @2
        global.get 1
        global.get 1
        i32.load
        i32.const 4
        i32.sub
        i32.store
        global.get 1
        i32.load
        i32.load
        local.set 3
      end
      global.get 0
      i32.eqz
      local.get 3
      i32.eqz
      i32.or
      if ;; label = @2
        i32.const 3
        i32.const 0
        i32.const 100
        i32.const 7
        i32.const 9
        i64.const 536870911
        i64.const 0
        i32.const 0
        i32.const 0
        call $path_open
        local.set 0
        i32.const 0
        global.get 0
        i32.const 1
        i32.eq
        br_if 1 (;@1;)
        drop
        local.get 0
        local.set 1
      end
      global.get 0
      i32.eqz
      if ;; label = @2
        local.get 1
        if ;; label = @3
          unreachable
        end
        i32.const 0
        i32.load
        local.set 1
      end
      loop ;; label = @2
        local.get 2
        local.get 2
        i32.const 1
        i32.add
        global.get 0
        select
        local.set 2
        global.get 0
        i32.eqz
        local.get 3
        i32.const 1
        i32.eq
        i32.or
        if ;; label = @3
          local.get 1
          local.get 2
          call $record
          i32.const 1
          global.get 0
          i32.const 1
          i32.eq
          br_if 2 (;@1;)
          drop
        end
        global.get 0
        i32.eqz
        local.get 3
        i32.const 2
        i32.eq
        i32.or
        if ;; label = @3
          call $sleep
          i32.const 2
          global.get 0
          i32.const 1
          i32.eq
          br_if 2 (;@1;)
          drop
        end
        global.get 0
        i32.eqz
        local.get 2
        i32.const 10
        i32.lt_u
        i32.and
        br_if 0 (;@2;)
      end
      return
    end
    local.set 0
    global.get 1
    i32.load
    local.get 0
    i32.store
    global.get 1
    global.get 1
    i32.load
    i32.const 4
    i32.add
    i32.store
    global.get 1
    i32.load
    local.tee 0
    local.get 1
    i32.store
    local.get 0
    local.get 2
    i32.store offset=4
    global.get 1
    global.get 1
    i32.load
    i32.const 8
    i32.add
    i32.store
  )
  (func (;7;) (type 2) (param i32)
    i32.const 1
    global.set 0
    local.get 0
    global.set 1
    global.get 1
    i32.load
    global.get 1
    i32.load offset=4
    i32.gt_u
    if ;; label = @1
      unreachable
    end
  )
  (func (;8;) (type 0)
    i32.const 0
    global.set 0
    global.get 1
    i32.load
    global.get 1
    i32.load offset=4
    i32.gt_u
    if ;; label = @1
      unreachable
    end
  )
  (func (;9;) (type 2) (param i32)
    i32.const 2
    global.set 0
    local.get 0
    global.set 1
    global.get 1
    i32.load
    global.get 1
    i32.load offset=4
    i32.gt_u
    if ;; label = @1
      unreachable
    end
  )
  (func (;10;) (type 6) (result i32)
    global.get 0
  )
  (data (;0;) (i32.const 100) "log.bin")
  (data (;1;) (i32.const 320) "\01")
  (data (;2;) (i32.const 328) "@B\0f")
)
