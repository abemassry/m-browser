;; Generated from count.wat by Binaryen's Asyncify: see that file. Do not edit.
(module
  (type (;0;) (func (param i32) (result i32)))
  (type (;1;) (func))
  (type (;2;) (func (result i32)))
  (type (;3;) (func (param i32)))
  (import "env" "tick" (func $tick (;0;) (type 0)))
  (memory (;0;) 1)
  (global $total (;0;) (mut i32) i32.const 0)
  (global (;1;) (mut i32) i32.const 0)
  (global (;2;) (mut i32) i32.const 0)
  (export "memory" (memory 0))
  (export "_start" (func $_start))
  (export "total" (func $get_total))
  (export "asyncify_start_unwind" (func 4))
  (export "asyncify_stop_unwind" (func 5))
  (export "asyncify_start_rewind" (func 6))
  (export "asyncify_stop_rewind" (func 5))
  (export "asyncify_get_state" (func 7))
  (func $inner (;1;) (type 0) (param i32) (result i32)
    (local i32 i32)
    block (result i32) ;; label = @1
      global.get 1
      i32.const 2
      i32.eq
      if (result i32) ;; label = @2
        global.get 2
        global.get 2
        i32.load
        i32.const 8
        i32.sub
        i32.store
        global.get 2
        i32.load
        local.tee 2
        i32.load
        local.set 0
        local.get 2
        i32.load offset=4
      else
        local.get 2
      end
      local.get 0
      i32.const 3
      i32.mul
      global.get 1
      select
      local.set 2
      global.get 1
      i32.eqz
      global.get 1
      i32.const 2
      i32.eq
      if (result i32) ;; label = @2
        global.get 2
        global.get 2
        i32.load
        i32.const 4
        i32.sub
        i32.store
        global.get 2
        i32.load
        i32.load
      else
        local.get 1
      end
      i32.eqz
      i32.or
      if ;; label = @2
        local.get 0
        call $tick
        local.set 1
        i32.const 0
        global.get 1
        i32.const 1
        i32.eq
        br_if 1 (;@1;)
        drop
        local.get 1
        local.set 0
      end
      global.get 1
      i32.eqz
      if ;; label = @2
        local.get 0
        local.get 2
        i32.add
        return
      end
      unreachable
    end
    local.set 1
    global.get 2
    i32.load
    local.get 1
    i32.store
    global.get 2
    global.get 2
    i32.load
    i32.const 4
    i32.add
    i32.store
    global.get 2
    i32.load
    local.tee 1
    local.get 0
    i32.store
    local.get 1
    local.get 2
    i32.store offset=4
    global.get 2
    global.get 2
    i32.load
    i32.const 8
    i32.add
    i32.store
    i32.const 0
  )
  (func $_start (;2;) (type 1)
    (local i32 i32 i32 i32 i32)
    global.get 1
    i32.const 2
    i32.eq
    if ;; label = @1
      global.get 2
      global.get 2
      i32.load
      i32.const 12
      i32.sub
      i32.store
      global.get 2
      i32.load
      local.tee 0
      i32.load
      local.set 2
      local.get 0
      i32.load offset=4
      local.set 3
      local.get 0
      i32.load offset=8
      local.set 0
    end
    block (result i32) ;; label = @1
      global.get 1
      i32.const 2
      i32.eq
      if ;; label = @2
        global.get 2
        global.get 2
        i32.load
        i32.const 4
        i32.sub
        i32.store
        global.get 2
        i32.load
        i32.load
        local.set 4
      end
      loop ;; label = @2
        local.get 2
        global.get $total
        global.get 1
        select
        local.set 2
        global.get 1
        i32.eqz
        local.get 4
        i32.eqz
        i32.or
        if ;; label = @3
          local.get 3
          call $inner
          local.set 1
          i32.const 0
          global.get 1
          i32.const 1
          i32.eq
          br_if 2 (;@1;)
          drop
          local.get 1
          local.set 0
        end
        global.get 1
        i32.eqz
        if ;; label = @3
          local.get 0
          local.get 2
          i32.add
          global.set $total
          i32.const 0
          global.get $total
          i32.store
          local.get 3
          i32.const 1
          i32.add
          local.tee 3
          i32.const 10
          i32.lt_u
          local.tee 2
          br_if 1 (;@2;)
        end
      end
      return
    end
    local.set 1
    global.get 2
    i32.load
    local.get 1
    i32.store
    global.get 2
    global.get 2
    i32.load
    i32.const 4
    i32.add
    i32.store
    global.get 2
    i32.load
    local.tee 1
    local.get 2
    i32.store
    local.get 1
    local.get 3
    i32.store offset=4
    local.get 1
    local.get 0
    i32.store offset=8
    global.get 2
    global.get 2
    i32.load
    i32.const 12
    i32.add
    i32.store
  )
  (func $get_total (;3;) (type 2) (result i32)
    global.get $total
  )
  (func (;4;) (type 3) (param i32)
    i32.const 1
    global.set 1
    local.get 0
    global.set 2
    global.get 2
    i32.load
    global.get 2
    i32.load offset=4
    i32.gt_u
    if ;; label = @1
      unreachable
    end
  )
  (func (;5;) (type 1)
    i32.const 0
    global.set 1
    global.get 2
    i32.load
    global.get 2
    i32.load offset=4
    i32.gt_u
    if ;; label = @1
      unreachable
    end
  )
  (func (;6;) (type 3) (param i32)
    i32.const 2
    global.set 1
    local.get 0
    global.set 2
    global.get 2
    i32.load
    global.get 2
    i32.load offset=4
    i32.gt_u
    if ;; label = @1
      unreachable
    end
  )
  (func (;7;) (type 2) (result i32)
    global.get 1
  )
)
