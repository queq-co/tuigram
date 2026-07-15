//! Neutralizing untrusted text before it can reach the terminal.
//!
//! Message text, captions, file names, titles, and sender names arrive from
//! other Telegram users and flow — via the model stores — into ratatui `Span`s.
//! Ratatui's buffer cells pass their bytes through to crossterm unaltered, and
//! crossterm writes them to the terminal verbatim. A raw ESC (`0x1B`) in that
//! stream is a terminal control introducer: a hostile message could retitle the
//! window, plant an OSC 8 hyperlink over innocent-looking text, or flood the
//! screen with control sequences. That is classic terminal escape injection
//! (CWE-150); see the 2023 survey at <https://dgl.cx/2023/09/ansi-terminal-security>.
//!
//! This module is the trust boundary. Every attacker-controlled string is
//! scrubbed here, at [`from_tdlib`](crate::model) projection time, so the model
//! stores never hold hostile bytes and no render path has to remember to
//! sanitize. Two policies, by field shape:
//!
//! - [`scrub_prose`] for multi-line bodies (message text, captions, poll text):
//!   keeps `\n` line structure and keeps Unicode bidi marks — right-to-left
//!   languages rely on them in ordinary prose — but replaces every control byte.
//! - [`scrub_line`] for single-line identifiers (file names, chat titles, user
//!   names, reaction emoji): additionally folds away line breaks and neutralizes
//!   the Unicode bidi *overrides*, which spoof file names Trojan-Source style
//!   (`report_e\u{202E}xe.txt` reads as `report_txt.exe`).
//!
//! Control bytes are *replaced* one-for-one with the Unicode replacement
//! character (U+FFFD) rather than stripped. This keeps a rendered marker where
//! tampering happened, and — because U+FFFD and every C0/C1/DEL control are each
//! a single UTF-16 code unit — it preserves the UTF-16 offsets that a
//! [`FormattedText`](crate::model::FormattedText)'s entities are counted in, so
//! formatting spans stay aligned over scrubbed text.

/// What a neutralized byte becomes: the Unicode replacement character. One
/// UTF-16 code unit, matching every control it replaces, so entity offsets over
/// scrubbed prose do not shift.
const REPLACEMENT: char = '\u{FFFD}';

/// Upper bound on a scrubbed prose field, in `char`s. Telegram caps messages at
/// 4096 and captions at 1024, so this sits far above any legitimate value; it
/// exists only to stop a forged multi-megabyte body from ballooning memory and
/// the per-repaint render allocations. Truncation lands on a `char` boundary.
pub const MAX_PROSE_CHARS: usize = 16_384;

/// Upper bound on a scrubbed single-line field (file name, title, name), in
/// `char`s — generous for any real identifier, tight enough to bound abuse.
pub const MAX_LINE_CHARS: usize = 1_024;

/// Neutralize a multi-line body (message text, caption, poll question) for
/// display: line breaks are preserved, tabs become a single space (a tab has no
/// defined width in a ratatui cell and would misalign the grid), every other C0
/// control, the C1 range, and DEL become `REPLACEMENT`, and the result is
/// capped at [`MAX_PROSE_CHARS`]. Bidi and zero-width characters are left as-is;
/// they are legitimate in prose and cannot, unlike ESC, introduce a terminal
/// control sequence.
///
/// ```
/// use tuigram_core::scrub_prose;
///
/// // A hostile OSC window-title escape is neutralized...
/// let clean = scrub_prose("before\u{1b}]0;pwned\u{07}after");
/// assert!(!clean.contains('\u{1b}'));
///
/// // ...but ordinary newlines survive, since prose keeps its line structure.
/// assert_eq!(scrub_prose("line one\nline two"), "line one\nline two");
/// ```
#[must_use]
pub fn scrub_prose(input: &str) -> String {
    scrub(input, MAX_PROSE_CHARS, Shape::Prose)
}

/// Neutralize a single-line identifier (file name, chat title, user name,
/// reaction emoji) for display: everything [`scrub_prose`] does, plus line
/// breaks become `REPLACEMENT` (a newline in a "file name" is a fake second
/// line) and Unicode bidi overrides are replaced (Trojan-Source-style spoofing).
/// Capped at [`MAX_LINE_CHARS`].
///
/// ```
/// use tuigram_core::scrub_line;
///
/// // The classic Trojan-Source filename spoof (`report_exe.txt` reading as
/// // `report_txt.exe`) loses its bidi override.
/// let clean = scrub_line("report_e\u{202e}xe.txt");
/// assert!(!clean.contains('\u{202e}'));
/// ```
#[must_use]
pub fn scrub_line(input: &str) -> String {
    scrub(input, MAX_LINE_CHARS, Shape::Line)
}

/// Which policy [`scrub`] applies — the two differ only in how they treat line
/// breaks and bidi overrides.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Multi-line body: keep `\n`, keep bidi.
    Prose,
    /// Single-line identifier: fold `\n`, neutralize bidi overrides.
    Line,
}

/// The shared scrubber. Iterates at most `max_chars` `char`s — which both bounds
/// the allocation and truncates on a `char` boundary — replacing control bytes
/// (and, for [`Shape::Line`], line breaks and bidi overrides) with
/// [`REPLACEMENT`].
fn scrub(input: &str, max_chars: usize, shape: Shape) -> String {
    let mut out = String::with_capacity(input.len().min(max_chars * 4));
    for ch in input.chars().take(max_chars) {
        let mapped = match ch {
            // Prose keeps its line breaks; a single-line field treats one as a
            // spoofed extra line and marks it.
            '\n' => {
                if shape == Shape::Prose {
                    '\n'
                } else {
                    REPLACEMENT
                }
            }
            // A tab has no well-defined cell width; render it as one space in
            // both shapes rather than let it misalign the grid.
            '\t' => ' ',
            // C0 controls (ESC above all), DEL, and the C1 range: never
            // legitimate display text, and the injection vector this exists for.
            c if c.is_control() => REPLACEMENT,
            // Bidi overrides reorder the surrounding run; harmless in RTL prose,
            // but in an identifier they disguise its true spelling.
            c if shape == Shape::Line && is_bidi_override(c) => REPLACEMENT,
            c => c,
        };
        out.push(mapped);
    }
    out
}

/// Whether `c` is a Unicode bidirectional *override* or *isolate* control: the
/// embedding/override block U+202A–U+202E and the isolate block U+2066–U+2069.
/// These reorder neighbouring characters and are the Trojan-Source spoofing
/// vector; the weaker directional *marks* (U+200E/U+200F) are left alone.
const fn is_bidi_override(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No control byte survives a prose scrub — the core anti-injection property.
    #[test]
    fn prose_replaces_every_control_byte() {
        let hostile = "before\u{1b}]0;pwned\u{07}after"; // OSC window-title set
        let clean = scrub_prose(hostile);
        assert!(
            !clean.chars().any(|c| c.is_control() && c != '\n'),
            "no control byte remains: {clean:?}"
        );
        assert_eq!(clean, "before\u{fffd}]0;pwned\u{fffd}after");
    }

    /// An OSC 8 hyperlink escape (clickable-link clickbait) is fully neutralized.
    #[test]
    fn prose_neutralizes_osc8_hyperlink() {
        let hostile = "\u{1b}]8;;https://evil.example\u{1b}\\click\u{1b}]8;;\u{1b}\\";
        let clean = scrub_prose(hostile);
        assert!(!clean.contains('\u{1b}'), "no ESC remains: {clean:?}");
    }

    /// A DECRQSS / bare-ESC probe leaves no escape introducer behind.
    #[test]
    fn prose_neutralizes_decrqss_probe() {
        let clean = scrub_prose("\u{1b}P$qm\u{1b}\\");
        assert!(!clean.contains('\u{1b}'));
    }

    /// Prose keeps its own newlines (the render path splits on them) and turns
    /// tabs into a single space.
    #[test]
    fn prose_keeps_newlines_and_spaces_tabs() {
        assert_eq!(scrub_prose("a\nb\tc"), "a\nb c");
    }

    /// Ordinary prose keeps bidi marks — RTL languages need them and they cannot
    /// introduce a terminal control sequence.
    #[test]
    fn prose_leaves_bidi_alone() {
        let rtl = "shalom \u{202b}\u{05e9}\u{05dc}\u{05d5}\u{05dd}\u{202c}";
        assert_eq!(scrub_prose(rtl), rtl);
    }

    /// A file name with a bidi override — the classic `exe`/`txt` swap — is
    /// neutralized so the extension cannot be disguised.
    #[test]
    fn line_neutralizes_bidi_spoofed_filename() {
        let spoof = "report_e\u{202e}xe.txt";
        let clean = scrub_line(spoof);
        assert!(!clean.contains('\u{202e}'), "override removed: {clean:?}");
        assert_eq!(clean, "report_e\u{fffd}xe.txt");
    }

    /// A single-line field folds an embedded newline into a marker rather than a
    /// spoofed extra line.
    #[test]
    fn line_folds_newlines() {
        assert_eq!(scrub_line("real.pdf\nHACKED"), "real.pdf\u{fffd}HACKED");
    }

    /// A pathological body is capped, bounding memory and the render loop; a
    /// zero-width flood is bounded by the same cap.
    #[test]
    fn prose_caps_length() {
        let flood = "\u{200b}".repeat(MAX_PROSE_CHARS * 4);
        assert_eq!(scrub_prose(&flood).chars().count(), MAX_PROSE_CHARS);
        let long = "x".repeat(MAX_PROSE_CHARS + 5000);
        assert_eq!(scrub_prose(&long).chars().count(), MAX_PROSE_CHARS);
    }

    /// A single-line field is capped tighter than prose.
    #[test]
    fn line_caps_length() {
        let long = "x".repeat(MAX_LINE_CHARS * 3);
        assert_eq!(scrub_line(&long).chars().count(), MAX_LINE_CHARS);
    }

    /// Replacing controls one-for-one preserves the UTF-16 length, so a
    /// `FormattedText`'s entity offsets still line up over scrubbed text.
    #[test]
    fn prose_preserves_utf16_offsets() {
        let hostile = "a\u{1b}b\u{07}c\td"; // tab also maps to one unit (space)
        assert_eq!(
            scrub_prose(hostile).encode_utf16().count(),
            hostile.encode_utf16().count(),
        );
    }

    /// Clean text passes through untouched (no needless churn or reallocation of
    /// meaning for the overwhelming common case).
    #[test]
    fn clean_text_is_unchanged() {
        let ok = "Hello, 世界! 🌍 https://example.com";
        assert_eq!(scrub_prose(ok), ok);
        assert_eq!(scrub_line("vacation-photo.jpg"), "vacation-photo.jpg");
    }
}
