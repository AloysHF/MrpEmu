//! Compact Lua 5.0 pattern matching over byte strings.
//!
//! Supports the character classes `%a %c %d %l %p %s %u %w %x %z`, `.`,
//! sets `[...]` (ranges, classes, and `^` negation), the quantifiers
//! `* + - ?`, the `^`/`$` anchors, and `%bxy` balanced matches. Captures
//! (`(...)`, `%1`) are not implemented; `find`/`gsub` operate on the whole
//! match, which covers the platform scripts' usage.

/// One parsed pattern element.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Any,
    Byte(u8),
    Class(u8),
    Set {
        ranges: Vec<SetEntry>,
        negated: bool,
    },
    Balanced(u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SetEntry {
    Byte(u8),
    Range(u8, u8),
    Class(u8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Quantifier {
    One,
    Star,
    Plus,
    Minus,
    Optional,
}

/// A parsed pattern: items plus the leading/trailing anchors.
struct Pattern {
    items: Vec<(Item, Quantifier)>,
    anchored_start: bool,
    anchored_end: bool,
}

impl Pattern {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let anchored_start = bytes.first() == Some(&b'^');
        let anchored_end = bytes.last() == Some(&b'$') && bytes.len() > 1;
        let body = &bytes[if anchored_start { 1 } else { 0 }
            ..bytes.len()
                - if anchored_end && bytes.len() > 1 {
                    1
                } else {
                    0
                }];
        let mut items = Vec::new();
        let mut pos = 0usize;
        while pos < body.len() {
            let item = match body[pos] {
                b'.' => {
                    pos += 1;
                    Item::Any
                }
                b'%' => {
                    let Some(&class) = body.get(pos + 1) else {
                        return Err("malformed pattern (dangling %)".to_string());
                    };
                    pos += 2;
                    if class == b'b' {
                        let (Some(&open), Some(&close)) = (body.get(pos), body.get(pos + 1)) else {
                            return Err("malformed %b balanced match".to_string());
                        };
                        pos += 2;
                        Item::Balanced(open, close)
                    } else if class.is_ascii_alphabetic() {
                        Item::Class(class.to_ascii_lowercase())
                    } else {
                        Item::Byte(class)
                    }
                }
                b'[' => {
                    pos += 1;
                    let negated = body.get(pos) == Some(&b'^');
                    if negated {
                        pos += 1;
                    }
                    let mut ranges = Vec::new();
                    loop {
                        let Some(&byte) = body.get(pos) else {
                            return Err("malformed pattern (unterminated set)".to_string());
                        };
                        pos += 1;
                        if byte == b']' {
                            break;
                        }
                        let low = if byte == b'%' {
                            let Some(&class) = body.get(pos) else {
                                return Err("malformed pattern (dangling %)".to_string());
                            };
                            pos += 1;
                            if class.is_ascii_alphabetic() {
                                ranges.push(SetEntry::Class(class.to_ascii_lowercase()));
                                continue;
                            }
                            class
                        } else {
                            byte
                        };
                        if body.get(pos) == Some(&b'-')
                            && body.get(pos + 1).is_some_and(|&next| next != b']')
                        {
                            let Some(&high) = body.get(pos + 1) else {
                                return Err("malformed pattern (dangling range)".to_string());
                            };
                            pos += 2;
                            ranges.push(SetEntry::Range(low, high));
                        } else {
                            ranges.push(SetEntry::Byte(low));
                        }
                    }
                    Item::Set { ranges, negated }
                }
                byte => {
                    pos += 1;
                    Item::Byte(byte)
                }
            };
            let quantifier = match body.get(pos) {
                Some(b'*') => {
                    pos += 1;
                    Quantifier::Star
                }
                Some(b'+') => {
                    pos += 1;
                    Quantifier::Plus
                }
                Some(b'-') => {
                    pos += 1;
                    Quantifier::Minus
                }
                Some(b'?') => {
                    pos += 1;
                    Quantifier::Optional
                }
                _ => Quantifier::One,
            };
            items.push((item, quantifier));
        }
        Ok(Self {
            items,
            anchored_start,
            anchored_end,
        })
    }

    fn entry_matches(entry: &SetEntry, byte: u8) -> bool {
        match entry {
            SetEntry::Byte(expected) => *expected == byte,
            SetEntry::Range(low, high) => byte >= *low && byte <= *high,
            SetEntry::Class(class) => class_matches(*class, byte),
        }
    }

    fn item_matches(item: &Item, byte: u8) -> bool {
        match item {
            Item::Any => true,
            Item::Byte(expected) => *expected == byte,
            Item::Class(class) => class_matches(*class, byte),
            Item::Set { ranges, negated } => {
                let inside = ranges.iter().any(|entry| Self::entry_matches(entry, byte));
                inside != *negated
            }
            Item::Balanced(..) => false,
        }
    }

    /// Matches the items against `text` starting at `tpos`; returns the end
    /// offset of the (first/greediest) match.
    fn match_from(&self, items: &[(Item, Quantifier)], text: &[u8], tpos: usize) -> Option<usize> {
        let Some(((item, quantifier), rest)) = items.split_first() else {
            return Some(tpos);
        };
        if let Item::Balanced(open, close) = item {
            if *quantifier != Quantifier::One || tpos >= text.len() || text[tpos] != *open {
                return None;
            }
            let mut depth = 0usize;
            let mut cursor = tpos;
            while cursor < text.len() {
                if text[cursor] == *open {
                    depth += 1;
                } else if text[cursor] == *close {
                    depth -= 1;
                    if depth == 0 {
                        return self.match_from(rest, text, cursor + 1);
                    }
                }
                cursor += 1;
            }
            return None;
        }
        match quantifier {
            Quantifier::One => {
                if tpos < text.len() && Self::item_matches(item, text[tpos]) {
                    self.match_from(rest, text, tpos + 1)
                } else {
                    None
                }
            }
            Quantifier::Optional => {
                if tpos < text.len() && Self::item_matches(item, text[tpos]) {
                    if let Some(end) = self.match_from(rest, text, tpos + 1) {
                        return Some(end);
                    }
                }
                self.match_from(rest, text, tpos)
            }
            Quantifier::Star => {
                let mut end = tpos;
                while end < text.len() && Self::item_matches(item, text[end]) {
                    end += 1;
                }
                loop {
                    if let Some(matched) = self.match_from(rest, text, end) {
                        return Some(matched);
                    }
                    if end == tpos {
                        return None;
                    }
                    end -= 1;
                }
            }
            Quantifier::Plus => {
                let mut end = tpos;
                while end < text.len() && Self::item_matches(item, text[end]) {
                    end += 1;
                }
                while end > tpos {
                    if let Some(matched) = self.match_from(rest, text, end) {
                        return Some(matched);
                    }
                    end -= 1;
                }
                None
            }
            Quantifier::Minus => {
                let mut end = tpos;
                loop {
                    if let Some(matched) = self.match_from(rest, text, end) {
                        return Some(matched);
                    }
                    if end >= text.len() || !Self::item_matches(item, text[end]) {
                        return None;
                    }
                    end += 1;
                }
            }
        }
    }

    /// Finds the first match at or after `init`; returns (start, end). A
    /// trailing `$` in the pattern requires the match to reach the end.
    fn find(&self, text: &[u8], init: usize) -> Option<(usize, usize)> {
        let mut start = init.min(text.len());
        loop {
            if let Some(end) = self.match_from(&self.items, text, start) {
                if !self.anchored_end || end == text.len() {
                    return Some((start, end));
                }
            }
            if self.anchored_start || start >= text.len() {
                return None;
            }
            start += 1;
        }
    }
}

fn class_matches(class: u8, byte: u8) -> bool {
    match class {
        b'a' => byte.is_ascii_alphabetic(),
        b'c' => byte.is_ascii_control(),
        b'd' => byte.is_ascii_digit(),
        b'l' => byte.is_ascii_lowercase(),
        b'p' => byte.is_ascii_punctuation(),
        b's' => byte.is_ascii_whitespace(),
        b'u' => byte.is_ascii_uppercase(),
        b'w' => byte.is_ascii_alphanumeric(),
        b'x' => byte.is_ascii_hexdigit(),
        b'z' => byte == 0,
        _ => false,
    }
}

/// `string.find` over the pattern engine; `plain` skips pattern parsing.
pub fn find(text: &[u8], pattern: &[u8], init: usize, plain: bool) -> Option<(usize, usize)> {
    if plain {
        if init > text.len() {
            return None;
        }
        let start = text[init..]
            .windows(pattern.len())
            .position(|window| window == pattern)
            .map(|offset| offset + init)?;
        return Some((start, start + pattern.len()));
    }
    Pattern::parse(pattern).ok()?.find(text, init)
}

/// Expands the replacement template for `gsub`: `%` escapes and `%d`
/// captures pass through literally (captures are unsupported), everything
/// else copies verbatim.
pub fn gsub(
    text: &[u8],
    pattern: &[u8],
    replacement: &[u8],
    max_replace: Option<usize>,
) -> Vec<u8> {
    let Ok(parsed) = Pattern::parse(pattern) else {
        return text.to_vec();
    };
    let mut out = Vec::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut replacements = 0usize;
    while cursor < text.len() {
        if max_replace.is_some_and(|limit| replacements >= limit) {
            break;
        }
        if let Some(end) = parsed.match_from(&parsed.items, text, cursor) {
            out.extend_from_slice(replacement);
            if end == cursor {
                // Empty match: copy one byte and advance to avoid looping.
                out.push(text[cursor]);
                cursor += 1;
            } else {
                cursor = end;
            }
            replacements += 1;
        } else {
            out.push(text[cursor]);
            cursor += 1;
        }
    }
    out.extend_from_slice(&text[cursor.min(text.len())..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_literal_and_anchors() {
        assert_eq!(find(b"hello world", b"world", 0, false), Some((6, 11)));
        assert_eq!(find(b"hello world", b"o w", 0, false), Some((4, 7)));
        assert_eq!(find(b"hello", b"z", 0, false), None);
        assert_eq!(find(b"aaa", b"^aa", 0, false), Some((0, 2)));
        assert_eq!(find(b"baa", b"^aa", 0, false), None);
        assert_eq!(find(b"abc", b"c$", 0, false), Some((2, 3)));
    }

    #[test]
    fn find_classes_and_sets() {
        assert_eq!(find(b"abc 123", b"%d+", 0, false), Some((4, 7)));
        assert_eq!(find(b"abc 123", b"%s%d", 0, false), Some((3, 5)));
        assert_eq!(find(b"hello", b"l+", 0, false), Some((2, 4)));
        assert_eq!(find(b"abc123", b"[0-9]+", 0, false), Some((3, 6)));
        assert_eq!(find(b"abc123", b"[^a-z]+", 0, false), Some((3, 6)));
        assert_eq!(find(b"a-b", b"%-%-", 0, false), None);
        assert_eq!(find(b"a--b", b"%-%-", 0, false), Some((1, 3)));
    }

    #[test]
    fn gsub_replaces_whole_matches() {
        assert_eq!(gsub(b"a1b2c3", b"%d", b"#", None), b"a#b#c#".to_vec());
        assert_eq!(gsub(b"aaa", b"a", b"b", Some(2)), b"bba".to_vec());
        assert_eq!(gsub(b"path\\x", b"\\", b"/", None), b"path/x".to_vec());
    }

    #[test]
    fn plain_find_skips_patterns() {
        assert_eq!(find(b"100%", b"%", 0, true), Some((3, 4)));
        assert_eq!(find(b"100%", b"%d", 0, true), None);
    }
}
