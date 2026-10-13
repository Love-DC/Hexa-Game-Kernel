(module
  (memory 1)
  (func (export "on_tick")
    (drop (i32.load offset=1048576 (i32.const 0)))))
