; Protobuf indents, in Aether's Helix-style vocabulary (`@indent` / `@outdent`). The crate's own
; `indents.scm` uses nvim's `@indent.begin` dialect, which the engine doesn't read.
[
  (message_body)
  (enum_body)
  (oneof)
  (service)
  (block_lit)
] @indent

; An rpc's options body has no node of its own, so match the rpc — but only one that has the
; body. One ended by `;` would otherwise indent the line after it.
(rpc "}") @indent

"}" @outdent
