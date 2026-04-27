//! Stable per-line hash anchors for the `read` and `edit` tools.
//!
//! The `read` tool emits each line prefixed with `<hash>§<line>` (or
//! `<hash>.<n>§<line>` for the 2nd+ occurrence of an identical line in a
//! single file). The `edit` tool accepts `anchor` / `end_anchor` strings —
//! either the bare token (`1a2b3c4d` / `1a2b3c4d.2`) or the full hashed
//! line copied straight from the read output — and re-validates them against
//! the file's current content before applying the edit.
//!
//! The hash is FNV-1a 32-bit, rendered as 8 lowercase hex chars. It is
//! computed from the line bytes (no trailing newline). This matches Dirac's
//! `contentHash` (see `dirac/src/utils/line-hashing.ts`).

/// The delimiter between an anchor token and its line content. U+00A7
/// (section sign) — virtually never appears in source code, well-handled
/// by tokenisers.
pub const ANCHOR_DELIMITER: char = '§';

/// FNV-1a 32-bit hash of a string, rendered as 8 lowercase hex chars.
pub fn fnv1a_8hex(s: &str) -> String {
    const OFFSET_BASIS: u32 = 0x811c9dc5;
    const PRIME: u32 = 0x01000193;
    let mut h: u32 = OFFSET_BASIS;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(PRIME);
    }
    format!("{:08x}", h)
}

/// Compute per-line anchor tokens for a slice of lines, with `.n`
/// disambiguators appended to the 2nd+ occurrence of any identical hash.
pub fn hash_lines_with_disambiguators(lines: &[&str]) -> Vec<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let h = fnv1a_8hex(line);
        let n = counts.entry(h.clone()).or_insert(0);
        *n += 1;
        if *n == 1 {
            out.push(h);
        } else {
            out.push(format!("{}.{}", h, n));
        }
    }
    out
}

/// Render lines with their anchor tokens prefixed, separated by
/// [`ANCHOR_DELIMITER`], joined with `\n`.
pub fn format_hashed(lines: &[&str], anchors: &[String]) -> String {
    debug_assert_eq!(lines.len(), anchors.len());
    let mut out = String::new();
    for (i, (anchor, line)) in anchors.iter().zip(lines.iter()).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(anchor);
        out.push(ANCHOR_DELIMITER);
        out.push_str(line);
    }
    out
}

/// Strip the line-content suffix from an anchor input, if present.
///
/// The model may pass either the bare token (`1a2b3c4d` / `1a2b3c4d.2`) or
/// the full hashed line copied from a read (`1a2b3c4d§    def foo():`).
/// Everything from the first [`ANCHOR_DELIMITER`] onward is dropped, and
/// the remainder is trimmed of surrounding whitespace.
pub fn extract_anchor_token(s: &str) -> &str {
    let cut = match s.find(ANCHOR_DELIMITER) {
        Some(i) => &s[..i],
        None => s,
    };
    cut.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_known_vectors() {
        // FNV-1a 32-bit reference vectors.
        assert_eq!(fnv1a_8hex(""), "811c9dc5");
        assert_eq!(fnv1a_8hex("a"), "e40c292c");
        assert_eq!(fnv1a_8hex("foobar"), "bf9cf968");
    }

    #[test]
    fn fnv1a_is_stable_across_runs() {
        // Sanity: same input → same output, twice.
        let a = fnv1a_8hex("    def foo():");
        let b = fnv1a_8hex("    def foo():");
        assert_eq!(a, b);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn disambiguator_starts_at_2() {
        let lines = vec!["foo", "bar", "foo", "foo", "baz"];
        let anchors = hash_lines_with_disambiguators(&lines);
        let h_foo = fnv1a_8hex("foo");
        let h_bar = fnv1a_8hex("bar");
        let h_baz = fnv1a_8hex("baz");
        assert_eq!(anchors[0], h_foo);
        assert_eq!(anchors[1], h_bar);
        assert_eq!(anchors[2], format!("{}.2", h_foo));
        assert_eq!(anchors[3], format!("{}.3", h_foo));
        assert_eq!(anchors[4], h_baz);
    }

    #[test]
    fn format_hashed_joins_with_delimiter() {
        let lines = vec!["alpha", "beta"];
        let anchors = hash_lines_with_disambiguators(&lines);
        let out = format_hashed(&lines, &anchors);
        let expected = format!("{}§alpha\n{}§beta", fnv1a_8hex("alpha"), fnv1a_8hex("beta"));
        assert_eq!(out, expected);
    }

    #[test]
    fn format_hashed_empty() {
        let lines: Vec<&str> = vec![];
        let anchors: Vec<String> = vec![];
        assert_eq!(format_hashed(&lines, &anchors), "");
    }

    #[test]
    fn extract_anchor_token_bare() {
        assert_eq!(extract_anchor_token("1a2b3c4d"), "1a2b3c4d");
        assert_eq!(extract_anchor_token("1a2b3c4d.2"), "1a2b3c4d.2");
        assert_eq!(extract_anchor_token("  1a2b3c4d  "), "1a2b3c4d");
    }

    #[test]
    fn extract_anchor_token_strips_full_line() {
        assert_eq!(extract_anchor_token("1a2b3c4d§    def foo():"), "1a2b3c4d");
        assert_eq!(extract_anchor_token("1a2b3c4d.2§duplicate"), "1a2b3c4d.2");
    }
}
