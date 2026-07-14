; HCL / Terraform highlights. The `tree-sitter-hcl` crate ships no queries, so this is vendored
; (adapted from the nvim-treesitter `hcl` query for the same grammar). Capture names are chosen to
; match Aether's theme vocabulary: attribute names, object keys, and member access are `@property`
; (nvim's `@variable.member` strips to bare `@variable`, which is unthemed in the GUI/web shells).

(comment) @comment

(numeric_lit) @number
(bool_lit) @boolean
(null_lit) @constant.builtin

[
  (quoted_template_start)
  (quoted_template_end)
  (template_literal)
] @string

[
  (heredoc_identifier)
  (heredoc_start)
] @string

[
  (template_interpolation_start)
  (template_interpolation_end)
  (template_directive_start)
  (template_directive_end)
  (strip_marker)
] @punctuation.special

[
  "for"
  "endfor"
  "in"
] @keyword

[
  "if"
  "else"
  "endif"
] @keyword

[
  "!"
  "\*"
  "/"
  "%"
  "\+"
  "-"
  ">"
  ">="
  "<"
  "<="
  "=="
  "!="
  "&&"
  "||"
] @operator

[
  (ellipsis)
  "\?"
  "=>"
] @punctuation.special

[
  "."
  ".*"
  ","
  "[*]"
] @punctuation.delimiter

[
  "{"
  "}"
  "["
  "]"
  "("
  ")"
] @punctuation.bracket

; Fallback: any identifier is a variable. More specific rules below (later patterns) win.
(identifier) @variable

; Top-level block type (resource, variable, module, ...); nested block types get @type.
(body
  (block
    (identifier) @keyword))

(body
  (block
    (body
      (block
        (identifier) @type))))

(function_call
  (identifier) @function.call)

; Attribute names (LHS of `name = value`) read as block properties.
(attribute
  (identifier) @property)

; Object element keys, highlighted like attributes.
(object_elem
  key: (expression
    (variable_expr
      (identifier) @property)))

; var.foo / local.bar / path.module: the leading namespace is builtin, the field is a member.
(expression
  (variable_expr
    (identifier) @variable.builtin)
  (get_attr
    (identifier) @property))

; Terraform primitive type constructors (used in `variable "x" { type = string }`).
((identifier) @type.builtin
  (#match? @type.builtin "^(bool|string|number|object|tuple|list|map|set|any)$"))
