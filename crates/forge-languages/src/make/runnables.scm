; Each target of a rule gets a run button (`make <target>`), except special targets
; (`.PHONY`…) and patterns (`%.o`), which can't be run by name.
((rule
  (targets
    (word) @run @_target))
  (#not-match? @_target "^\\.|%")
  (#set! tag make-target))
