//! Role-based colour themes — the single source of truth for every client's palette.
//!
//! Two layers:
//!
//! - **Shades** — the fixed Nord constants (`NORD0`…`NORD15` plus the derived in-between
//!   shades). These never change between themes; https://www.nordtheme.com/.
//! - **Roles** — what a colour *means*: background, panel, accent, error. [`Theme`] is one
//!   `role → shade` table per [`ThemeMode`]; call sites reference roles only, so a theme is
//!   ~40 assignments here rather than a second palette in every shell.
//!
//! Shells convert [`Rgb`] to their own colour type at the edge (ratatui `Color::Rgb`, iced
//! `Color`, CSS custom properties — `web/src/theme.css` hand-mirrors these tables as
//! `--role` variables, one block per `[data-theme]`; keep them in sync). The semantic
//! mappings that used to be hand-mirrored per shell live here too: tree-sitter capture →
//! [`SyntaxStyle`], diagnostic severity / LSP status / git status → role.
//!
//! [`Theme::DARK`] maps every role to the shade the clients hardcoded before themes existed,
//! so dark renders pixel-identical to the pre-theme editor. [`Theme::LIGHT`]'s off-palette
//! values (darkened Frost/Aurora accents — the originals sit at ~2–3:1 against Snow Storm and
//! wash out; pale tint bands) are first-pass values tuned on the web client; treat them as
//! provisional in the way the dark values are not.

use aether_protocol::git::GitStatus;
use aether_protocol::lsp::LspStatus;
use aether_protocol::settings::ThemeMode;
use aether_protocol::viewport::DiagnosticSeverity;

/// A palette colour, sRGB 8-bit. The core stays shell-agnostic: shells convert to their own
/// colour type at the draw boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub const fn rgb(hex: u32) -> Rgb {
    Rgb {
        r: ((hex >> 16) & 0xff) as u8,
        g: ((hex >> 8) & 0xff) as u8,
        b: (hex & 0xff) as u8,
    }
}

impl Rgb {
    /// `#rrggbb`, for the web shell and for pinning values in tests.
    pub fn css(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

// ---- Shades ------------------------------------------------------------------------------------
// The Nord palette, exactly as the three shells carried it (plus NORD5, which only the light
// theme uses). Derived tints that were tuned against one background (git line tints, the
// cursorline family) are not shades — they live per-theme in the tables below.

pub const NORD0: Rgb = rgb(0x2e3440); // Polar Night — darkest
pub const NORD1: Rgb = rgb(0x3b4252);
pub const NORD2: Rgb = rgb(0x434c5e);
pub const NORD3: Rgb = rgb(0x4c566a);
/// Off-palette Polar Night extension — "lighter dim" (legible secondary text on dark panels).
pub const NORD3_BRIGHT: Rgb = rgb(0x616e88);
/// Off-palette, brighter still — the dark theme's syntax-comment shade: NORD3 comments were
/// ~1.4:1 against the reading view's NORD1 code panels; this reaches ~2.8:1 there (~3.5:1 on
/// the editor) while staying below NORD9 keywords, so comments stay the dimmest rung.
pub const NORD3_BRIGHTER: Rgb = rgb(0x7b88a1);
pub const NORD4: Rgb = rgb(0xd8dee9); // Snow Storm
pub const NORD5: Rgb = rgb(0xe5e9f0);
pub const NORD6: Rgb = rgb(0xeceff4); // Snow Storm — brightest
pub const NORD7: Rgb = rgb(0x8fbcbb); // Frost — teal
pub const NORD8: Rgb = rgb(0x88c0d0); // Frost — cyan
pub const NORD9: Rgb = rgb(0x81a1c1); // Frost — light blue
pub const NORD10: Rgb = rgb(0x5e81ac); // Frost — deep blue
pub const NORD11: Rgb = rgb(0xbf616a); // Aurora — red
pub const NORD12: Rgb = rgb(0xd08770); // Aurora — orange
pub const NORD13: Rgb = rgb(0xebcb8b); // Aurora — yellow
pub const NORD14: Rgb = rgb(0xa3be8c); // Aurora — green
pub const NORD15: Rgb = rgb(0xb48ead); // Aurora — purple

/// One theme: every colour role the shells draw with. Fields are grouped; the dark column is the
/// pre-theme constant each shell carried, named in the comments so the mapping stays auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// The mode this table renders — for the rare places a shell must branch on the mode itself
    /// (the web shell's `data-theme` attribute, iced's built-in base theme for widget chrome).
    pub mode: ThemeMode,

    // ---- Backgrounds ----
    /// Editor / app background.
    pub bg: Rgb,
    /// Status line, panels, picker surfaces.
    pub bg_panel: Rgb,
    /// Chrome selection: picker active row, chips, in-chrome selections.
    pub bg_selection: Rgb,
    /// Editor visual-mode selection.
    pub bg_visual: Rgb,
    /// Muted fills: search-hit tint, the sneak word band, scroll tracks.
    pub fill_dim: Rgb,
    /// Sneak typed-prefix band — between [`Self::fill_dim`] and the label cell in prominence.
    pub sneak_prefix_bg: Rgb,
    /// Fuzzy-match/sneak emphasis: matched characters in picker rows, the sneak label hue.
    /// Shares dark's shade with [`Self::warning`] but is match emphasis, not a severity — the
    /// two tune independently.
    pub match_highlight: Rgb,
    /// The paired-bracket highlight under the cursor. Shares dark's shade with
    /// [`Self::syn_macro`]; chrome, not syntax.
    pub match_bracket: Rgb,
    /// Current-line tint — between `bg` and `bg_panel` (Nord has no shade in between).
    pub cursor_line_bg: Rgb,
    /// Outline for floating overlays' frames and separators.
    pub overlay_border: Rgb,
    /// Hairline borders on chrome panels (inputs, dialogs, toasts, table frames) — a step
    /// quieter than [`Self::overlay_border`].
    pub border_subtle: Rgb,

    // ---- Foregrounds ----
    /// Main text.
    pub fg: Rgb,
    /// Brightest text: headings, the search query, file labels.
    pub fg_bright: Rgb,
    /// Dim-but-legible chrome text a rung above [`Self::fg_dim`] — picker metadata, input
    /// placeholders, the tether mark, completed-task prose. Shares dark's shade with
    /// [`Self::syn_comment`] but is chrome, not syntax: the two tune independently.
    /// (Foreground ladder, brightest first: `fg_bright` > `fg` > `fg_muted` > `fg_dim` >
    /// `fg_faint`.)
    pub fg_muted: Rgb,
    /// Legible secondary text on panels.
    pub fg_dim: Rgb,
    /// Barely-there text: ignored entries, muted glyphs, disabled chrome.
    pub fg_faint: Rgb,
    /// Text sitting on an accent/selection fill (the cursor cell, selected headers) — the dark
    /// end of the palette in dark mode, the light end in light mode.
    pub fg_on_accent: Rgb,
    /// Ghost/example text in prompts (the save-prompt path hint). Deliberately off-palette in
    /// dark — a touch brighter than [`Self::fg_muted`] so it reads as an actionable example
    /// rather than disabled chrome.
    pub ghost_text: Rgb,

    // ---- Accents & status ----
    /// The interaction accent: cursor, focused elements, chrome highlights.
    pub accent: Rgb,
    /// Secondary accent: branch/meta text in the status bar; markdown links in the native
    /// clients.
    pub accent_alt: Rgb,
    /// Deep accent used as a fill (active selection headers, focused buttons).
    pub accent_deep: Rgb,
    pub error: Rgb,
    pub warning: Rgb,
    pub info: Rgb,
    pub ok: Rgb,

    // ---- Buffer-state dot (status bar + web favicon) ----
    /// Gone on disk.
    pub state_deleted: Rgb,
    /// Changed on disk under the buffer.
    pub state_changed: Rgb,
    /// Unsaved edits.
    pub state_unsaved: Rgb,

    // ---- Git ----
    // Hue says what changed; the dimmed "staged" variants say it's already in the index.
    pub git_added: Rgb,
    pub git_modified: Rgb,
    pub git_deleted: Rgb,
    pub git_staged_added: Rgb,
    pub git_staged_modified: Rgb,
    pub git_staged_deleted: Rgb,
    // Inline-diff line tints (and phantom deleted rows' backgrounds).
    pub git_added_bg: Rgb,
    pub git_modified_bg: Rgb,
    pub git_deleted_bg: Rgb,
    pub git_staged_added_bg: Rgb,
    pub git_staged_modified_bg: Rgb,
    pub git_staged_deleted_bg: Rgb,
    // Intra-line diff emphasis: a stronger fill over the corresponding line tint marking the
    // sub-line ranges a change actually touched. Only Modified lines and phantom deleted rows
    // carry emphasis, so there is no added variant. Sits below search/selection fills.
    pub git_modified_emph_bg: Rgb,
    pub git_deleted_emph_bg: Rgb,
    pub git_staged_modified_emph_bg: Rgb,
    pub git_staged_deleted_emph_bg: Rgb,
    // Cursorline variants on changed lines, so the cursorline doesn't hide the change colour.
    pub cursor_line_added_bg: Rgb,
    pub cursor_line_modified_bg: Rgb,
    pub cursor_line_staged_added_bg: Rgb,
    pub cursor_line_staged_modified_bg: Rgb,

    // ---- Markdown reading view ----
    /// Code spans/blocks panel.
    pub md_code_bg: Rgb,
    /// Alternating table-row band, between `bg` and `md_code_bg`.
    pub md_table_stripe_bg: Rgb,
    /// The "Important" alert's hue. The other four alert kinds are genuine statuses (note =
    /// [`Self::info`], tip = [`Self::ok`], warning, caution = [`Self::error`]); purple has no
    /// status meaning, so it gets its own role rather than borrowing [`Self::syn_constant`].
    pub md_alert_important: Rgb,

    // ---- Syntax (tree-sitter capture roles; applied via [`Theme::syntax`]) ----
    pub syn_keyword: Rgb,
    pub syn_string: Rgb,
    pub syn_string_special: Rgb,
    pub syn_comment: Rgb,
    pub syn_constant: Rgb,
    pub syn_function: Rgb,
    pub syn_macro: Rgb,
    pub syn_type: Rgb,
}

impl Theme {
    /// The role table every client rendered as constants before themes existed — dark stays
    /// pixel-identical.
    pub const DARK: Theme = Theme {
        mode: ThemeMode::Dark,
        bg: NORD0,
        bg_panel: NORD1,
        bg_selection: NORD2,
        bg_visual: NORD10,
        fill_dim: NORD3,
        sneak_prefix_bg: NORD3_BRIGHT,
        match_highlight: NORD13,
        match_bracket: NORD12,
        cursor_line_bg: rgb(0x343a48), // ~40% from NORD0 toward NORD1
        overlay_border: NORD3_BRIGHTER,
        border_subtle: NORD3,
        fg: NORD4,
        fg_bright: NORD6,
        fg_muted: NORD3_BRIGHTER,
        fg_dim: NORD3_BRIGHT,
        fg_faint: NORD3,
        fg_on_accent: NORD0,
        ghost_text: rgb(0x8c96a5), // off-palette; see the role doc
        accent: NORD8,
        accent_alt: NORD9,
        accent_deep: NORD10,
        error: NORD11,
        warning: NORD13,
        info: NORD8,
        ok: NORD14,
        state_deleted: NORD11,
        state_changed: NORD12,
        state_unsaved: NORD9,
        git_added: NORD14,
        git_modified: NORD13,
        git_deleted: NORD11,
        git_staged_added: rgb(0x6e8060),    // dimmed NORD14
        git_staged_modified: rgb(0x9e8a62), // dimmed NORD13
        git_staged_deleted: rgb(0x844c53),  // dimmed NORD11
        git_added_bg: rgb(0x2d3a2d),
        git_modified_bg: rgb(0x3a3628),
        git_deleted_bg: rgb(0x3b2226),
        git_staged_added_bg: rgb(0x2f3631),
        git_staged_modified_bg: rgb(0x35342d),
        git_staged_deleted_bg: rgb(0x33252a),
        // Emphasis must read against BOTH the plain tint and the cursor-line variant of its kind
        // (the first-pass values sat almost on the cursor-line tints and vanished there). Derived
        // as the kind hue mixed further into its own tint (NORD13 into the modified tint, NORD11
        // into the phantom bg) so the fill reads as a stronger wash of the same glass, not a
        // solid pigment.
        git_modified_emph_bg: rgb(0x665b41),
        git_deleted_emph_bg: rgb(0x65363c),
        git_staged_modified_emph_bg: rgb(0x544e3d),
        git_staged_deleted_emph_bg: rgb(0x503339),
        cursor_line_added_bg: rgb(0x3a4d3a),
        cursor_line_modified_bg: rgb(0x4a4632),
        cursor_line_staged_added_bg: rgb(0x3a453c),
        cursor_line_staged_modified_bg: rgb(0x434138),
        md_code_bg: NORD1,
        md_table_stripe_bg: rgb(0x323845), // between NORD0 and NORD1
        md_alert_important: NORD15,
        syn_keyword: NORD9,
        syn_string: NORD14,
        syn_string_special: NORD13,
        syn_comment: NORD3_BRIGHTER,
        syn_constant: NORD15,
        syn_function: NORD8,
        syn_macro: NORD12,
        syn_type: NORD7,
    };

    /// Nord light: Polar Night and Snow Storm swap ends, Frost/Aurora accents darken to hold
    /// contrast on the pale backgrounds. The salience ladders (comment below keyword; staged
    /// dimmer than unstaged; stripe between bg and panel) keep their dark-theme ordering.
    pub const LIGHT: Theme = Theme {
        mode: ThemeMode::Light,
        bg: NORD6,
        bg_panel: NORD5,
        bg_selection: NORD4,
        bg_visual: rgb(0xc2d6e7), // pale Frost — dark text stays readable inside a selection
        fill_dim: rgb(0xd8dfe8),
        sneak_prefix_bg: rgb(0xc4cedb), // darker than fill_dim: prominence inverts on light
        match_highlight: rgb(0x9a7522), // = warning today; free to diverge
        match_bracket: rgb(0xab5f38),   // = syn_macro today; free to diverge
        cursor_line_bg: rgb(0xe4e9f0),  // ~40% from NORD6 toward NORD5
        overlay_border: rgb(0xaab4c4),
        border_subtle: rgb(0xd8dfe8), // = fill_dim today; borders can darken independently
        fg: NORD0,
        fg_bright: rgb(0x242933), // a step below NORD0, as NORD6 sits a step above NORD4
        fg_muted: rgb(0x8892a4),  // = syn_comment today; free to diverge
        fg_dim: NORD3,
        fg_faint: rgb(0xaeb7c6),
        fg_on_accent: NORD6,
        ghost_text: rgb(0x8a94a6),
        accent: rgb(0x3e7a8f), // NORD8 darkened (~1.9:1 on NORD6 as-is)
        accent_alt: NORD10,
        accent_deep: NORD10,
        error: NORD11,
        warning: rgb(0x9a7522), // NORD13 darkened — yellow is unreadable on Snow Storm
        info: rgb(0x3e7a8f),
        ok: rgb(0x5a7547), // NORD14 darkened
        state_deleted: NORD11,
        state_changed: rgb(0xab5f38), // NORD12 darkened
        state_unsaved: NORD10,
        git_added: rgb(0x5a7547),
        git_modified: rgb(0x9a7522),
        git_deleted: NORD11,
        git_staged_added: rgb(0x8ca378), // staged = lifted toward bg, mirroring dark's dimming
        git_staged_modified: rgb(0xbb9d66),
        git_staged_deleted: rgb(0xcc8a91),
        git_added_bg: rgb(0xdcebd4),
        git_modified_bg: rgb(0xeee8cd),
        git_deleted_bg: rgb(0xf2d8da),
        git_staged_added_bg: rgb(0xe3ecdf),
        git_staged_modified_bg: rgb(0xebe7d8),
        git_staged_deleted_bg: rgb(0xefe0e2),
        git_modified_emph_bg: rgb(0xe3c366),
        git_deleted_emph_bg: rgb(0xf1abb0),
        git_staged_modified_emph_bg: rgb(0xe4d29a),
        git_staged_deleted_emph_bg: rgb(0xf0c1c6),
        cursor_line_added_bg: rgb(0xd3e4cb),
        cursor_line_modified_bg: rgb(0xe6dfc0),
        cursor_line_staged_added_bg: rgb(0xdde7d8),
        cursor_line_staged_modified_bg: rgb(0xe7e3d2),
        md_code_bg: rgb(0xe1e6ee),
        md_table_stripe_bg: rgb(0xe9edf3),
        md_alert_important: rgb(0x8d6488), // = syn_constant today; free to diverge
        syn_keyword: NORD10,
        syn_string: rgb(0x5a7547),
        syn_string_special: rgb(0x9a7522),
        syn_comment: rgb(0x8892a4), // dimmest rung: below NORD10 keywords on NORD6, like dark
        syn_constant: rgb(0x8d6488), // NORD15 darkened
        syn_function: rgb(0x3e7a8f),
        syn_macro: rgb(0xab5f38), // NORD12 darkened
        syn_type: rgb(0x4f8886),  // NORD7 darkened
    };

    pub fn of(mode: ThemeMode) -> &'static Theme {
        match mode {
            ThemeMode::Dark => &Self::DARK,
            ThemeMode::Light => &Self::LIGHT,
        }
    }

    /// Style for a tree-sitter highlight name, falling back along dotted prefixes
    /// (`function.method.call` → `function`). `None` means the kind is unknown — render as the
    /// default foreground. `Some` with `color: None` carries attributes only (emphasis/strong,
    /// plain `variable`).
    pub fn syntax(&self, kind: &str) -> Option<SyntaxStyle> {
        let mut k = kind;
        loop {
            if let Some(style) = self.syntax_exact(k) {
                return Some(style);
            }
            match k.rfind('.') {
                Some(dot) => k = &k[..dot],
                None => return None,
            }
        }
    }

    fn syntax_exact(&self, kind: &str) -> Option<SyntaxStyle> {
        let color = |c: Rgb| SyntaxStyle {
            color: Some(c),
            ..SyntaxStyle::default()
        };
        Some(match kind {
            "keyword" | "variable.builtin" | "operator" | "tag" => color(self.syn_keyword),
            "string" | "text.literal" => color(self.syn_string),
            "string.escape" | "string.special" => color(self.syn_string_special),
            "comment" => SyntaxStyle {
                italic: true,
                ..color(self.syn_comment)
            },
            "number" | "boolean" | "constant" | "constant.builtin" => color(self.syn_constant),
            "function" | "function.call" | "text.reference" => color(self.syn_function),
            "function.macro" | "punctuation.special" | "attribute" | "label" => {
                color(self.syn_macro)
            }
            "type" | "type.builtin" | "module" | "namespace" | "constructor" => {
                color(self.syn_type)
            }
            "variable" => SyntaxStyle::default(),
            "variable.parameter" | "punctuation.bracket" | "punctuation.delimiter" | "property" => {
                color(self.fg)
            }
            // Markdown (tree-sitter-md's "text.*" capture names).
            "text.title" => SyntaxStyle {
                bold: true,
                ..color(self.syn_function)
            },
            "text.uri" => SyntaxStyle {
                underline: true,
                ..color(self.syn_function)
            },
            "text.emphasis" => SyntaxStyle {
                italic: true,
                ..SyntaxStyle::default()
            },
            "text.strong" => SyntaxStyle {
                bold: true,
                ..SyntaxStyle::default()
            },
            _ => return None,
        })
    }

    /// The underline / message colour for a diagnostic severity. Hint is deliberately the plain
    /// foreground, not a hue: readable on panels and distinct from the coloured severities.
    pub fn diagnostic(&self, severity: DiagnosticSeverity) -> Rgb {
        match severity {
            DiagnosticSeverity::Error => self.error,
            DiagnosticSeverity::Warning => self.warning,
            DiagnosticSeverity::Information => self.info,
            DiagnosticSeverity::Hint => self.fg,
        }
    }

    /// State colour for a language-server's status dot. The transitional states read as "busy";
    /// a ready server with in-flight `$/progress` should show [`Self::warning`] — the caller
    /// checks `progress`.
    pub fn lsp_status(&self, status: &LspStatus) -> Rgb {
        match status {
            LspStatus::Ready => self.ok,
            LspStatus::Starting | LspStatus::Initializing | LspStatus::Restarting => self.warning,
            LspStatus::Crashed { .. } => self.error,
            LspStatus::Stopped => self.fg_faint,
        }
    }

    /// Status-bullet colour for a git file status: green for new, yellow for modified, red for
    /// removed/conflict. `None` for ignored entries — they carry no bullet (ignored is dimmed
    /// via its text colour instead).
    pub fn git_status_bullet(&self, status: GitStatus) -> Option<Rgb> {
        match status {
            GitStatus::Added | GitStatus::Untracked => Some(self.git_added),
            GitStatus::Modified => Some(self.git_modified),
            GitStatus::Deleted | GitStatus::Conflicted => Some(self.git_deleted),
            GitStatus::Ignored => None,
        }
    }
}

/// Resolved style for a tree-sitter capture: a role colour plus font attributes. Shells that
/// can't render an attribute (no italics in some terminals) drop it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyntaxStyle {
    pub color: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dark must stay pixel-identical to the constants the shells carried before themes existed.
    #[test]
    fn dark_pins_the_pre_theme_constants() {
        let t = Theme::DARK;
        assert_eq!(t.bg.css(), "#2e3440");
        assert_eq!(t.bg_panel.css(), "#3b4252");
        assert_eq!(t.fg.css(), "#d8dee9");
        assert_eq!(t.accent.css(), "#88c0d0");
        assert_eq!(t.cursor_line_bg.css(), "#343a48");
        assert_eq!(t.git_staged_modified.css(), "#9e8a62");
        assert_eq!(t.syn_comment.css(), "#7b88a1");
        assert_eq!(t.md_table_stripe_bg.css(), "#323845");
        // The chrome roles that share a dark shade with a semantically unrelated role — the
        // shade is pinned here so renaming call sites can never shift dark rendering.
        assert_eq!(t.match_highlight, NORD13);
        assert_eq!(t.match_bracket, NORD12);
        assert_eq!(t.md_alert_important, NORD15);
        assert_eq!(t.fg_muted, NORD3_BRIGHTER);
        assert_eq!(t.border_subtle, NORD3);
        assert_eq!(t.ghost_text.css(), "#8c96a5");
    }

    #[test]
    fn syntax_falls_back_along_dotted_prefixes() {
        let t = Theme::DARK;
        // "function.method.call" isn't listed; it falls back to "function".
        assert_eq!(t.syntax("function.method.call"), t.syntax("function"));
        assert_eq!(t.syntax("function").unwrap().color, Some(t.syn_function));
        // Unknown kinds resolve to the default foreground.
        assert_eq!(t.syntax("nonsense"), None);
        // Emphasis carries attributes but no colour of its own.
        let em = t.syntax("text.emphasis").unwrap();
        assert_eq!(em.color, None);
        assert!(em.italic);
        // Comments are italic in every theme.
        assert!(t.syntax("comment").unwrap().italic);
    }

    /// Both tables answer every role: spot-check that light diverges where it must (backgrounds
    /// swap ends, yellow darkens) and that the mode tag matches the table.
    #[test]
    fn light_is_a_distinct_full_table() {
        let d = Theme::DARK;
        let l = Theme::LIGHT;
        assert_eq!(d.mode, ThemeMode::Dark);
        assert_eq!(l.mode, ThemeMode::Light);
        assert_eq!(Theme::of(ThemeMode::Light).bg, l.bg);
        assert_eq!(l.bg, NORD6);
        assert_eq!(l.fg, NORD0);
        assert_ne!(l.warning, NORD13, "aurora yellow is unreadable on light");
        // The staged-dimmer-than-unstaged ladder holds in both directions.
        assert_ne!(l.git_added, l.git_staged_added);
    }

    #[test]
    fn semantic_helpers_map_through_roles() {
        let t = Theme::LIGHT;
        assert_eq!(t.diagnostic(DiagnosticSeverity::Error), t.error);
        assert_eq!(t.diagnostic(DiagnosticSeverity::Hint), t.fg);
        assert_eq!(t.lsp_status(&LspStatus::Ready), t.ok);
        assert_eq!(t.lsp_status(&LspStatus::Stopped), t.fg_faint);
        assert_eq!(t.git_status_bullet(GitStatus::Ignored), None);
        assert_eq!(t.git_status_bullet(GitStatus::Untracked), Some(t.git_added));
    }
}
