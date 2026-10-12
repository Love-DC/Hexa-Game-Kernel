(module
  (import "gk" "counter_read" (func $counter_read (result i64)))
  (import "gk" "counter_set" (func $counter_set (param i64)))
  (func (export "on_tick")
    call $counter_read
    i64.const 1
    i64.add
    call $counter_set))
