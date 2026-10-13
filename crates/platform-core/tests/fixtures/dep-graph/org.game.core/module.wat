;; Resolver fixture module. It only has to exist so the package can be loaded;
;; the dependency graph tests never instantiate it.
(module
  (func (export "on_tick")))
