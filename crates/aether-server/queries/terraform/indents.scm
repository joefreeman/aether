; HCL / Terraform indents (vendored from Helix's `hcl` query — same grammar). Uses Aether's
; Helix-style indent vocabulary (`@indent` / `@outdent`).
[
  (object)
  (block)
  (tuple)
  (for_tuple_expr)
  (for_object_expr)
] @indent

[
  (object_end)
  (block_end)
  (tuple_end)
] @outdent
