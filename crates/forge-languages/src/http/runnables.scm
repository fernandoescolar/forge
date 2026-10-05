; Each request line gets a run button that sends it (forge-http handles the click).
((request_line) @run
  (#set! tag http-request))
