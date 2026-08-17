; Commit messages, in this codebase's capture vocabulary rather than the grammar's own
; (which uses nvim's `markup.*` names — `theme.rs` maps `text.*`, so upstream's query would
; resolve to nothing and render the file flat).
;
; Deliberately coarse. The whole point is separating what the *user wrote* from the comment
; block git generated: everything under a comment stays comment-coloured, including the branch
; name and the staged file list, because it is all going to be stripped before the commit lands.

(comment) @comment
(generated_comment) @comment

; Everything below the scissor line is a diff git appended for reference; it is not the message.
(scissor) @comment

; The subject line is the part that shows up in `git log --oneline`, so it earns emphasis.
(subject) @text.title

; Conventional-commit prefixes (`feat(api)!:`), when the user writes them.
(prefix (type) @keyword)
(prefix (scope) @variable.parameter)
(prefix ["(" ")" ":"] @punctuation.delimiter)
(prefix "!" @punctuation.special)

; `Signed-off-by:`, `Co-authored-by:` — structure worth seeing in a wall of prose.
(trailer (token) @label)

; `BREAKING CHANGE:` is the one thing in a message with consequences beyond the message.
(breaking_change (token) @keyword)
