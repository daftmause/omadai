use crate::secret::Secret;
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::Zeroizing;

/// Text that has been through [`scrub`]; the only kind of error text the cache stores or the CLI shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scrubbed(String);

impl Scrubbed {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Cuts the text to at most `max` bytes on a char boundary, marking the cut; still scrubbed.
    pub fn truncated(self, max: usize) -> Scrubbed {
        if self.0.len() <= max {
            return self;
        }
        let mut end = max;
        while !self.0.is_char_boundary(end) {
            end -= 1;
        }
        Scrubbed(format!("{}{TRUNCATED}", &self.0[..end]))
    }
}

pub const TRUNCATED: &str = "…[truncated]";

impl fmt::Display for Scrubbed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub const REDACTED: &str = "[redacted]";

/// Replaces with `[redacted]` every form of each credential, its `:`-parts and their 8-character prefixes, as written or behind JSON escapes up to [`DECODE_LEVELS`] deep.
pub fn scrub(text: &str, secrets: &[&Secret]) -> Scrubbed {
    Scrubber::new(secrets).scrub(text)
}

/// The error-text needles for many strings, built once so the secrets they came from can be dropped.
pub struct Scrubber(Vec<Zeroizing<String>>);

impl Scrubber {
    pub fn new(secrets: &[&Secret]) -> Self {
        Scrubber(needles(secrets, true))
    }

    pub fn scrub(&self, text: &str) -> Scrubbed {
        scrub_with(text, &self.0)
    }
}

fn scrub_with(text: &str, needles: &[Zeroizing<String>]) -> Scrubbed {
    // Every match, literal or decoded, is located in the original text first and redacted in one pass, so one match cannot hide another.
    let mut budget = DECODE_BUDGET;
    let Some(mut ranges) = literal_matches(text, needles, &mut budget) else {
        return Scrubbed(TOO_DENSE.to_string());
    };
    match escaped_matches(text, needles, &mut budget) {
        Ok(escaped) => ranges.extend(escaped),
        Err(Unsearchable::Dense) => return Scrubbed(TOO_DENSE.to_string()),
        Err(Unsearchable::Escaped) => return Scrubbed(TOO_ESCAPED.to_string()),
    }
    Scrubbed(redact_ranges(text, outside_markers(text, ranges)))
}

/// The needles for the id and name check, built once per fetch rather than once per string.
pub struct KeyCheck(Vec<Zeroizing<String>>);

impl KeyCheck {
    pub fn new(secrets: &[&Secret]) -> Self {
        KeyCheck(needles(secrets, false))
    }

    /// Whether `text` holds key material, so the fetch saves nothing rather than keep it; text too dense or too escaped to search counts as holding it.
    pub fn holds(&self, text: &str) -> bool {
        let mut budget = DECODE_BUDGET;
        match (
            literal_matches(text, &self.0, &mut budget),
            escaped_matches(text, &self.0, &mut budget),
        ) {
            (Some(literal), Ok(escaped)) => !literal.is_empty() || !escaped.is_empty(),
            _ => true,
        }
    }
}

/// Why [`escaped_matches`] could not search a text: too many matches for the budget, or escapes it could not decode within the budget or levels.
enum Unsearchable {
    Dense,
    Escaped,
}

/// Every form of each credential to search for, longest first; the id and name check (`with_public` false) skips prefixes and the first `:`-part, which every stored credential shape keeps public.
fn needles(secrets: &[&Secret], with_public: bool) -> Vec<Zeroizing<String>> {
    // Every needle is a copy of secret material, so each lives in its own zeroizing buffer.
    let mut needles: Vec<Zeroizing<String>> = Vec::new();
    for secret in secrets {
        let full = secret.expose();
        let mut whole = vec![full];
        let parts: Vec<&str> = secret.parts().collect();
        let public = !with_public && parts.len() > 1;
        whole.extend(parts.into_iter().skip(usize::from(public)));
        for item in whole {
            let escaped = json_escaped(item);
            let mut forms = vec![Zeroizing::new(item.to_string())];
            if escaped.as_str() != item {
                forms.push(escaped);
            }
            for i in 0..forms.len() {
                if forms[i].contains('/') {
                    let solidus = solidus_escaped(&forms[i]);
                    forms.push(solidus);
                }
            }
            for form in forms {
                if with_public {
                    needles.push(Zeroizing::new(prefix8(&form).to_string()));
                }
                needles.push(form);
            }
        }
    }
    needles.retain(|n| !n.is_empty());
    needles.sort_by(|a, b| b.len().cmp(&a.len()).then(a.as_str().cmp(b.as_str())));
    needles.dedup();
    needles
}

/// `ranges` with every match lying inside a marker an earlier scrub wrote dropped, and every match that only overlaps one widened to cover it, so scrubbing scrubbed text again changes nothing.
fn outside_markers(text: &str, ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut markers: Vec<(usize, usize)> = [REDACTED, TOO_DENSE, TOO_ESCAPED]
        .iter()
        .flat_map(|m| text.match_indices(m).map(|(at, m)| (at, at + m.len())))
        .collect();
    if markers.is_empty() {
        return ranges;
    }
    // Sorted and merged once, so each match finds the markers it touches by binary search rather than by a scan of all of them.
    markers.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(markers.len());
    for (s, e) in markers {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    ranges
        .into_iter()
        .filter_map(|(mut start, mut end)| {
            let mut i = merged.partition_point(|&(_, me)| me <= start);
            while let Some(&(ms, me)) = merged.get(i) {
                if ms >= end {
                    break;
                }
                if ms <= start && end <= me {
                    return None;
                }
                start = start.min(ms);
                end = end.max(me);
                i += 1;
            }
            Some((start, end))
        })
        .collect()
}

/// How many times escape sequences are decoded, so a key escaped inside a JSON string that itself holds JSON is still found.
pub const DECODE_LEVELS: usize = 3;

/// Bytes of ranges, decoded text and segment maps one scrub may hold, charged per decode level, so a body past about 4 MiB (1.4 MiB at three levels) with any escape becomes [`TOO_ESCAPED`].
pub const DECODE_BUDGET: usize = 4 << 20;

/// What a text becomes when its literal matches alone would pass [`DECODE_BUDGET`].
pub const TOO_DENSE: &str = "[redacted: response holds too many matches to scrub safely]";

/// What a text becomes when decoding it would pass [`DECODE_BUDGET`] or leave escapes after [`DECODE_LEVELS`], since a partly scrubbed text could still hold a key.
pub const TOO_ESCAPED: &str = "[redacted: response too heavily escaped to scrub safely]";

/// Byte ranges of `text` holding any needle as written, each charged to `budget`; None once it would run out.
fn literal_matches(
    text: &str,
    needles: &[Zeroizing<String>],
    budget: &mut usize,
) -> Option<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    for needle in needles {
        for (at, _) in text.match_indices(needle.as_str()) {
            *budget = budget.checked_sub(RANGE_COST)?;
            ranges.push((at, at + needle.len()));
        }
    }
    Some(ranges)
}

const RANGE_COST: usize = std::mem::size_of::<(usize, usize)>();

/// A stretch of decoded text: from `decoded` on, either copied byte for byte from `source` (`escape` false) or one char decoded from the escape at `source..source_end`.
#[derive(Clone, Copy)]
struct Segment {
    decoded: usize,
    source: usize,
    source_end: usize,
    escape: bool,
}

/// The source byte range that decoded byte `at` came from, given the segments of one decoding level.
fn source_of(segments: &[Segment], at: usize) -> (usize, usize) {
    let seg = segments[segments.partition_point(|s| s.decoded <= at) - 1];
    if seg.escape {
        (seg.source, seg.source_end)
    } else {
        let byte = seg.source + (at - seg.decoded);
        (byte, byte + 1)
    }
}

/// Source byte ranges of `text` that hold a needle once JSON escape sequences are decoded, at each level, or why the text could not be searched.
fn escaped_matches(
    text: &str,
    needles: &[Zeroizing<String>],
    budget: &mut usize,
) -> Result<Vec<(usize, usize)>, Unsearchable> {
    let mut ranges = Vec::new();
    // One segment list per level, each mapping that level's decoded text back to the level below it.
    let mut levels: Vec<Vec<Segment>> = Vec::new();
    let mut current: Option<Zeroizing<String>> = None;
    for _ in 0..DECODE_LEVELS {
        let Some((next, segments)) =
            decode_escapes(current.as_deref().map_or(text, |c| c.as_str()), budget)
                .ok_or(Unsearchable::Escaped)?
        else {
            break;
        };
        levels.push(segments);
        for needle in needles {
            for (at, _) in next.match_indices(needle.as_str()) {
                let (mut start, mut end) = (at, at + needle.len());
                for segments in levels.iter().rev() {
                    let first = source_of(segments, start).0;
                    end = source_of(segments, end - 1).1;
                    start = first;
                }
                *budget = budget.checked_sub(RANGE_COST).ok_or(Unsearchable::Dense)?;
                ranges.push((start, end));
            }
        }
        current = Some(next);
    }
    // Escapes still left after the last level could spell a key no level saw, so the whole text fails closed.
    if levels.len() == DECODE_LEVELS {
        let last = current.as_deref().map_or(text, |c| c.as_str()).as_bytes();
        if (0..last.len()).any(|i| last[i] == b'\\' && escape_at(last, i).is_some()) {
            return Err(Unsearchable::Escaped);
        }
    }
    Ok(ranges)
}

/// `text` with every valid JSON escape sequence decoded, and the segments mapping it back to `text`, charged to `budget`.
/// `Some(None)` where no valid escape exists, found before anything is allocated; `None` once `budget` would run out.
fn decode_escapes(
    text: &str,
    budget: &mut usize,
) -> Option<Option<(Zeroizing<String>, Vec<Segment>)>> {
    let bytes = text.as_bytes();
    let Some(first) =
        (0..bytes.len()).find(|&i| bytes[i] == b'\\' && escape_at(bytes, i).is_some())
    else {
        return Some(None);
    };
    *budget = budget.checked_sub(text.len())?;
    let mut out = Zeroizing::new(String::with_capacity(text.len()));
    let mut segments = Vec::new();
    let mut run = 0;
    let mut i = first;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some((c, len)) = escape_at(bytes, i) else {
            i += 1;
            continue;
        };
        let pushes = if run < i { 2 } else { 1 };
        *budget = budget.checked_sub(pushes * std::mem::size_of::<Segment>())?;
        if run < i {
            segments.push(Segment {
                decoded: out.len(),
                source: run,
                source_end: i,
                escape: false,
            });
            out.push_str(&text[run..i]);
        }
        segments.push(Segment {
            decoded: out.len(),
            source: i,
            source_end: i + len,
            escape: true,
        });
        out.push(c);
        i += len;
        run = i;
    }
    if run < bytes.len() {
        *budget = budget.checked_sub(std::mem::size_of::<Segment>())?;
        segments.push(Segment {
            decoded: out.len(),
            source: run,
            source_end: bytes.len(),
            escape: false,
        });
        out.push_str(&text[run..]);
    }
    Some(Some((out, segments)))
}

/// The char a JSON escape sequence starting at `bytes[i]` (a backslash) stands for, and its length in bytes.
fn escape_at(bytes: &[u8], i: usize) -> Option<(char, usize)> {
    let simple = match *bytes.get(i + 1)? {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'u' => {
            let high = hex4(bytes, i + 2)?;
            if let Some(c) = char::from_u32(high) {
                return Some((c, 6));
            }
            // A high surrogate counts only with the low one that follows it.
            let low = (bytes.get(i + 6) == Some(&b'\\') && bytes.get(i + 7) == Some(&b'u'))
                .then(|| hex4(bytes, i + 8))
                .flatten()?;
            if !(0xD800..0xDC00).contains(&high) || !(0xDC00..0xE000).contains(&low) {
                return None;
            }
            let c = char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00))?;
            return Some((c, 12));
        }
        _ => return None,
    };
    Some((simple, 2))
}

fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
    let digits = std::str::from_utf8(bytes.get(at..at + 4)?).ok()?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// `text` with each of `ranges` (merged where they overlap) replaced by `[redacted]`.
fn redact_ranges(text: &str, mut ranges: Vec<(usize, usize)>) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    ranges.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let mut iter = ranges.into_iter().peekable();
    while let Some((start, mut end)) = iter.next() {
        while let Some(&(s, e)) = iter.peek()
            && s <= end
        {
            end = end.max(e);
            iter.next();
        }
        out.push_str(&text[at..start]);
        out.push_str(REDACTED);
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// `text` as it appears inside a JSON string that serde_json encoded.
fn json_escaped(text: &str) -> Zeroizing<String> {
    // Sized for the longest escape (`\u00XX`) up front, so no reallocation leaves a copy behind.
    let mut quoted = Zeroizing::new(Vec::with_capacity(text.len() * 6 + 2));
    serde_json::to_writer(&mut *quoted, text).expect("a str serializes");
    let inner = std::str::from_utf8(&quoted[1..quoted.len() - 1]).expect("serde_json writes UTF-8");
    Zeroizing::new(inner.to_string())
}

/// `text` with each `/` written as `\/`, as some JSON encoders emit it.
fn solidus_escaped(text: &str) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(
        text.len() + text.matches('/').count(),
    ));
    for c in text.chars() {
        if c == '/' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// For text that carries no credential by construction, such as a local I/O error.
pub fn plain(text: impl Into<String>) -> Scrubbed {
    Scrubbed(text.into())
}

/// `text` with each control character (C0, DEL and C1, so also ESC) and each invisible or reordering format character written as its Rust escape, for a terminal.
pub fn printable(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(unprintable) {
        return text.into();
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_debug());
        } else if unprintable(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out.into()
}

/// [`printable`] applied to each line of `text`, keeping its line breaks.
pub fn printable_lines(text: &str) -> String {
    text.lines().map(printable).collect::<Vec<_>>().join("\n")
}

/// Control characters, and the characters that reorder, hide or break text: bidi controls, zero-width and other invisible format characters, Hangul fillers, variation selectors and their supplement, shorthand format controls, tag characters, the BOM, the specials block and the line and paragraph separators.
fn unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{ad}'
                | '\u{34f}'
                | '\u{61c}'
                | '\u{180b}'..='\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{600}'..='\u{605}'
                | '\u{6dd}'
                | '\u{70f}'
                | '\u{890}'..='\u{891}'
                | '\u{8e2}'
                | '\u{2060}'..='\u{206f}'
                | '\u{115f}'..='\u{1160}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff0}'..='\u{fffc}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e007f}'
                | '\u{e0100}'..='\u{e01ef}'
        )
}

fn prefix8(s: &str) -> &str {
    match s.char_indices().nth(8) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_full_key_and_prefix() {
        let key = Secret::new("fk-admin-ABCDEFGHIJKLMNOP".into());
        let body =
            r#"{"error":"invalid key fk-admin-ABCDEFGHIJKLMNOP","hint":"starts with fk-admin"}"#;
        let out = scrub(body, &[&key]);
        assert!(!out.as_str().contains("ABCDEFGH"), "{out}");
        assert!(!out.as_str().contains("fk-admin"), "{out}");
        assert!(out.as_str().contains("invalid key [redacted]"), "{out}");
    }

    #[test]
    fn removes_each_part_of_id_secret() {
        let key = Secret::new("fk-a1b2c3d4e5f6:fk-SECRETPARTxyz123456".into());
        let body = "id fk-a1b2c3d4e5f6 rejected; secret fk-SECRETPARTxyz123456 bad; prefix fk-SECRE; idprefix fk-a1b2c";
        let out = scrub(body, &[&key]);
        for leak in ["fk-a1b2c", "fk-SECRE", "xyz123456"] {
            assert!(!out.as_str().contains(leak), "{leak} in {out}");
        }
        assert!(out.as_str().starts_with("id [redacted]"), "{out}");
    }

    #[test]
    fn removes_access_key_and_secret_key_parts() {
        let pair = Secret::join(&[
            &Secret::new("fk-akexampleAK".into()),
            &Secret::new("fk-skexampleSecretValue".into()),
        ]);
        let out = scrub(
            "AccessKeyId fk-akexampleAK invalid, sig from fk-skexampleSecretValue",
            &[&pair],
        );
        assert!(!out.as_str().contains("fk-akexa"), "{out}");
        assert!(!out.as_str().contains("fk-skexa"), "{out}");
    }

    #[test]
    fn removes_json_escaped_key_and_parts() {
        let key = Secret::new("fk-q\"uo\\te\u{1}ESCAPEDTAIL:fk-p\"art\\TWOTAIL".into());
        let body = serde_json::json!({ "error": format!("bad key {}", key.expose()) }).to_string();
        let part = serde_json::json!({ "id": "fk-p\"art\\TWOTAIL" }).to_string();
        for text in [body, part] {
            let out = scrub(&text, &[&key]);
            for leak in ["ESCAPEDTAIL", "TWOTAIL", "fk-q", "fk-p"] {
                assert!(!out.as_str().contains(leak), "{leak} in {out}");
            }
            assert!(out.as_str().contains(REDACTED), "{out}");
        }
    }

    #[test]
    fn removes_solidus_escaped_key_parts_and_prefixes() {
        let key = Secret::new("fk/sol/IDPART:fk/sl/SECRETPARTTAIL".into());
        for text in [
            r#"{"error":"bad key fk\/sol\/IDPART:fk\/sl\/SECRETPARTTAIL"}"#,
            r#"{"id":"fk\/sol\/IDPART"}"#,
            r#"{"secret":"fk\/sl\/SECRETPARTTAIL"}"#,
            r#"{"hint":"starts with fk\/sol\/I or fk\/sl\/SE"}"#,
        ] {
            let out = scrub(text, &[&key]);
            for leak in ["IDPART", "SECRETPARTTAIL", "fk\\/sol\\/I", "fk\\/sl\\/SE"] {
                assert!(!out.as_str().contains(leak), "{leak} in {out}");
            }
            assert!(out.as_str().contains(REDACTED), "{out}");
        }
    }

    /// Each char of `s` as a JSON `\uXXXX` escape.
    fn u_escaped(s: &str) -> String {
        s.chars().map(|c| format!("\\u{:04x}", c as u32)).collect()
    }

    #[test]
    fn removes_fully_u_escaped_key() {
        let key = Secret::new("fk-unicodeESCAPEDTAIL".into());
        let body = format!(r#"{{"error":"bad key {}"}}"#, u_escaped(key.expose()));
        let out = scrub(&body, &[&key]);
        for leak in ["ESCAPEDTAIL", "\\u0066\\u006b", "fk-"] {
            assert!(!out.as_str().contains(leak), "{leak} in {out}");
        }
        assert!(out.as_str().contains(REDACTED), "{out}");
    }

    #[test]
    fn removes_mixed_escaped_and_plain_key() {
        let key = Secret::new("fk-mixedHALFandHALFTAIL".into());
        let (head, tail) = key.expose().split_at(9);
        let body = format!(r#"{{"error":"bad key {}{tail}","n":1}}"#, u_escaped(head));
        let out = scrub(&body, &[&key]);
        for leak in ["HALFTAIL", "andHALF", "\\u0066\\u006b"] {
            assert!(!out.as_str().contains(leak), "{leak} in {out}");
        }
        assert!(out.as_str().contains(REDACTED), "{out}");
    }

    #[test]
    fn removes_solidus_and_u_escaped_key_part_in_value_and_key() {
        let key = Secret::new("fk/id/IDPART:fk/sl/SECRETPARTTAIL".into());
        let written = format!("fk\\/sl\\/{}TAIL", u_escaped("SECRETPART"));
        for body in [
            format!(r#"{{"secret":"{written}"}}"#),
            format!(r#"{{"{written}":"seen as a key"}}"#),
        ] {
            let out = scrub(&body, &[&key]);
            for leak in ["SECRETPART", "\\u0053\\u0045", "fk\\/sl", "fk/sl"] {
                assert!(!out.as_str().contains(leak), "{leak} in {out}");
            }
            assert!(out.as_str().contains(REDACTED), "{out}");
        }
    }

    #[test]
    fn non_json_text_is_matched_as_before() {
        let key = Secret::new("fk-plainNONJSONTAIL".into());
        let truncated = r#"{"error":"bad key fk-plainNONJSONTAIL" and then the body was cut"#;
        assert_eq!(
            scrub(truncated, &[&key]).as_str(),
            r#"{"error":"bad key [redacted]" and then the body was cut"#
        );
        let untouched = r#"{"b":"x\/y","a":"A"}"#;
        assert_eq!(scrub(untouched, &[&key]).as_str(), untouched);
    }

    fn assert_clean(out: &Scrubbed, leaks: &[&str]) {
        for leak in leaks {
            assert!(!out.as_str().contains(leak), "{leak} in {out}");
        }
        assert!(out.as_str().contains(REDACTED), "{out}");
    }

    #[test]
    fn removes_u_escaped_key_in_text_that_is_not_json() {
        let key = Secret::new("fk-lenientESCAPEDTAIL".into());
        let u = u_escaped(key.expose());
        let leaks = ["ESCAPEDTAIL", "\\u0066\\u006b", "\\u0045\\u0053"];
        for body in [
            format!(r#"HTTP 401: {{"error":"bad key {u}"}}"#),
            format!(r#"{{"error":"bad key {u}"#),
            format!("\u{feff}{{\"error\":\"bad key {u}\"}}"),
            format!(")]}}'\n{{\"error\":\"bad key {u}\"}}"),
            format!("{{\"a\":1}}\n{{\"error\":\"bad key {u}\"}}\n"),
        ] {
            assert_clean(&scrub(&body, &[&key]), &leaks);
        }
    }

    #[test]
    fn removes_key_escaped_inside_a_json_string_holding_json() {
        let key = Secret::new("fk-doubleENCODEDTAIL".into());
        let twice: String = key
            .expose()
            .chars()
            .map(|c| format!("\\\\u{:04x}", c as u32))
            .collect();
        let body = format!(r#"{{"error":"{{\"k\":\"{twice}\"}}"}}"#);
        assert_clean(&scrub(&body, &[&key]), &["ENCODEDTAIL", "u0066", "u0045"]);
    }

    #[test]
    fn removes_key_written_with_surrogate_pairs() {
        let key = Secret::new("fk-\u{1f600}SURROGATETAIL".into());
        let body = r#"{"error":"bad key \u0066\u006b-\ud83d\ude00\u0053URROGATETAIL"}"#;
        assert!(
            !body.contains("fk-"),
            "the key is escaped, so only decoding finds it"
        );
        assert_clean(&scrub(body, &[&key]), &["URROGATETAIL", "\\ud83d"]);
    }

    #[test]
    fn text_with_nothing_to_redact_keeps_its_bytes() {
        let key = Secret::new("fk-absentKEYTAIL".into());
        for body in [
            r#"{"b":"x\/y","a":"é","z":[1,2]}"#,
            r#"HTTP 500: {"error":"a \"quoted\" \\ thing!"#,
        ] {
            assert_eq!(scrub(body, &[&key]).as_str(), body);
        }
    }

    #[test]
    fn redacts_only_the_escaped_key_and_keeps_the_rest_as_written() {
        let key = Secret::new("fk-keepRESTTAIL".into());
        let body = format!(
            r#"HTTP 401: {{"b":"x\/y","error":"bad {}"}}"#,
            u_escaped(key.expose())
        );
        assert_eq!(
            scrub(&body, &[&key]).as_str(),
            r#"HTTP 401: {"b":"x\/y","error":"bad [redacted]"}"#
        );
    }

    #[test]
    fn removes_key_holding_a_literal_escape_after_its_prefix() {
        // The key's own characters include a backslash and an n, which the decoded view reads as a newline.
        let key = Secret::new(r"fk-12345\nSECRETTail".into());
        for text in [
            r"invalid key fk-12345\nSECRETTail".to_string(),
            r#"{"error":"invalid key fk-12345\\nSECRETTail"}"#.to_string(),
            r"key fk-12345\nSECRETTail and again fk-12345\nSECRETTail".to_string(),
        ] {
            let out = scrub(&text, &[&key]);
            for leak in ["SECRETTail", "fk-12345"] {
                assert!(!out.as_str().contains(leak), "{leak} in {out}");
            }
            assert!(out.as_str().contains(REDACTED), "{out}");
        }
    }

    #[test]
    fn large_plain_body_builds_no_decode_map() {
        let key = Secret::new("fk-big-ABCDEFGHIJKLMNOP".into());
        let mut body = "x".repeat(8 << 20);
        body.push_str(" fk-big-ABCDEFGHIJKLMNOP");
        assert!(
            decode_escapes(&body, &mut DECODE_BUDGET.clone())
                .unwrap()
                .is_none()
        );
        let out = scrub(&body, &[&key]);
        assert!(!out.as_str().contains("ABCDEFGH"));
        assert!(out.as_str().ends_with(" [redacted]"));
    }

    #[test]
    fn removes_u_escaped_key_in_the_first_of_duplicate_json_keys() {
        let key = Secret::new("fk-admin-ABCDEFGHIJKLMNOP".into());
        let escaped: String = key
            .expose()
            .chars()
            .map(|c| format!("\\u{:04x}", c as u32))
            .collect();
        let body = format!(r#"{{"msg":"{escaped}","msg":"ok"}}"#);
        let out = scrub(&body, &[&key]);
        assert!(!out.as_str().contains(&escaped[..48]), "{out}");
        assert!(out.as_str().contains(r#""msg":"ok""#), "{out}");
        assert!(out.as_str().contains(REDACTED), "{out}");
    }

    #[test]
    fn a_key_escaped_deeper_than_the_decode_levels_is_replaced_whole() {
        let key = Secret::new("fk-deep-ABCDEFGHIJKLMNOP".into());
        // Each extra layer doubles the backslash, so after three decodes the text still holds the escape `\u0066` rather than the key, which is what fails it closed.
        let deep: String = u_escaped(key.expose()).replace('\\', &"\\".repeat(8));
        let body = format!(r#"{{"error":"bad key {deep}"}}"#);
        let out = scrub(&body, &[&key]);
        assert_eq!(out.as_str(), TOO_ESCAPED);
    }

    #[test]
    fn three_levels_of_escaping_with_nothing_left_still_scrubs_normally() {
        let key = Secret::new("fk-three-ABCDEFGHIJKLMNOP".into());
        let three: String = u_escaped(key.expose()).replace('\\', &"\\".repeat(4));
        let body = format!(r#"{{"error":"bad key {three}","ok":true}}"#);
        let out = scrub(&body, &[&key]);
        assert_ne!(out.as_str(), TOO_ESCAPED);
        assert!(out.as_str().contains(REDACTED), "{out}");
        assert!(out.as_str().contains(r#""ok":true"#), "{out}");
    }

    #[test]
    fn an_escape_heavy_body_past_the_budget_is_replaced_whole() {
        let key = Secret::new("fk-big-ABCDEFGHIJKLMNOP".into());
        let body = "\\n".repeat(4 << 20);
        let out = scrub(&body, &[&key]);
        assert_eq!(out.as_str(), TOO_ESCAPED);
    }

    #[test]
    fn a_backslash_that_starts_no_escape_allocates_nothing() {
        let mut body = "x".repeat(8 << 20);
        body.push_str("C:\\Users\\x");
        assert!(
            decode_escapes(&body, &mut DECODE_BUDGET.clone())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn many_markers_and_many_matches_scrub_in_near_linear_time() {
        let key = Secret::new("fk-admin-SWEEPTAILKEY".into());
        let mut text = String::new();
        for _ in 0..100_000 {
            text.push_str("[redacted] ");
        }
        for _ in 0..100_000 {
            text.push_str("fk-admin ");
        }
        let started = std::time::Instant::now();
        let out = scrub(&text, &[&key]);
        assert!(
            // The quadratic scan this replaced took about two minutes here, so the bound catches it without timing a loaded machine closely.
            started.elapsed() < std::time::Duration::from_secs(60),
            "took {:?}",
            started.elapsed()
        );
        assert!(!out.as_str().contains("fk-admin"));
    }

    #[test]
    fn printable_escapes_the_arabic_letter_mark() {
        assert_eq!(printable("a\u{61c}b"), "a\\u{61c}b");
    }

    #[test]
    fn printable_escapes_line_and_paragraph_separators() {
        assert_eq!(printable("a\u{2028}b\u{2029}c"), "a\\u{2028}b\\u{2029}c");
    }

    #[test]
    fn printable_escapes_format_characters() {
        assert_eq!(
            printable("key\u{202e}lab\u{2066}x\u{200b}y\u{feff}"),
            "key\\u{202e}lab\\u{2066}x\\u{200b}y\\u{feff}"
        );
    }

    #[test]
    fn printable_escapes_control_characters_only() {
        assert_eq!(
            printable("a\u{1b}[31mred\u{7f}\u{9b}2J\tb\né"),
            "a\\u{1b}[31mred\\u{7f}\\u{9b}2J\\tb\\né"
        );
        assert!(matches!(
            printable("plain €"),
            std::borrow::Cow::Borrowed("plain €")
        ));
    }

    #[test]
    fn truncated_cuts_on_a_char_boundary() {
        let s = plain("ab€cd").truncated(3);
        assert_eq!(s.as_str(), format!("ab{TRUNCATED}"));
        assert_eq!(plain("short").truncated(5).as_str(), "short");
    }

    #[test]
    fn short_secret_and_non_ascii() {
        let key = Secret::new("ключ".into());
        assert_eq!(scrub("bad ключ!", &[&key]).as_str(), "bad [redacted]!");
        let empty = Secret::new(String::new());
        assert_eq!(scrub("unchanged", &[&empty]).as_str(), "unchanged");
    }

    #[test]
    fn a_body_dense_with_matches_past_the_budget_is_replaced_whole() {
        let key = Secret::new("fk-admin-DENSEKEYTAIL".into());
        let body = "fk-admin ".repeat(1 << 20);
        let out = scrub(&body, &[&key]);
        assert_eq!(out.as_str(), TOO_DENSE);
    }

    #[test]
    fn escaped_matches_past_the_budget_are_dense_not_escaped() {
        let key = Secret::new("fk-admin-ESCDENSETAIL".into());
        let body = "\\u0066k-admin-ESCDENSETAIL ".repeat(64);
        let needles = needles(&[&key], true);
        // Some budget lets decoding finish but not every match, which must read as too dense.
        let dense = (0..body.len() * 64).any(|b| {
            let mut budget = b;
            matches!(
                escaped_matches(&body, &needles, &mut budget),
                Err(Unsearchable::Dense)
            )
        });
        assert!(dense);
    }

    #[test]
    fn a_body_past_the_decode_budget_with_one_escape_fails_closed_and_one_below_it_scrubs() {
        let key = Secret::new("fk-admin-CUTOFFKEYTAIL".into());
        let over = format!(
            "{}\\\" fk-admin-CUTOFFKEYTAIL",
            "x".repeat(DECODE_BUDGET + 1)
        );
        assert_eq!(scrub(&over, &[&key]).as_str(), TOO_ESCAPED);
        let under = format!(
            "{}\\\" fk-admin-CUTOFFKEYTAIL",
            "x".repeat(DECODE_BUDGET / 2)
        );
        let out = scrub(&under, &[&key]);
        assert!(
            out.as_str().ends_with(&format!("\\\" {REDACTED}")),
            "{}",
            &out.as_str()[out.as_str().len() - 40..]
        );
    }

    #[test]
    fn scrubbing_scrubbed_text_again_changes_nothing() {
        let key = Secret::new("d:fk-long-secret-tail-xyz".into());
        let once = scrub("Wallet d:fk-long-secret-tail-xyz", &[&key]);
        assert_eq!(once.as_str(), format!("Wallet {REDACTED}"));
        assert_eq!(scrub(once.as_str(), &[&key]), once);
        for marker in [TOO_ESCAPED, TOO_DENSE] {
            assert_eq!(scrub(marker, &[&key]).as_str(), marker);
        }
    }

    #[test]
    fn printable_escapes_the_remaining_invisible_format_characters() {
        for c in [
            '\u{206a}',
            '\u{206f}',
            '\u{600}',
            '\u{605}',
            '\u{6dd}',
            '\u{70f}',
            '\u{8e2}',
            '\u{110bd}',
            '\u{110cd}',
            '\u{1d173}',
            '\u{1d17a}',
            '\u{13430}',
            '\u{1343f}',
            '\u{fffc}',
        ] {
            let s = format!("a{c}b");
            let out = printable(&s);
            assert!(!out.contains(c), "{c:?} kept: {out:?}");
        }
    }

    #[test]
    fn printable_escapes_hangul_fillers_specials_and_supplementary_selectors() {
        for c in [
            '\u{890}',
            '\u{891}',
            '\u{115f}',
            '\u{1160}',
            '\u{3164}',
            '\u{ffa0}',
            '\u{fff0}',
            '\u{fff8}',
            '\u{1bca0}',
            '\u{1bca3}',
            '\u{e0100}',
            '\u{e01ef}',
        ] {
            let s = format!("a{c}b");
            let out = printable(&s);
            assert!(!out.contains(c), "{c:?} kept: {out:?}");
        }
    }

    #[test]
    fn printable_escapes_soft_hyphens_selectors_and_tag_characters() {
        for c in [
            '\u{ad}',
            '\u{34f}',
            '\u{180b}',
            '\u{180e}',
            '\u{fe00}',
            '\u{fe0f}',
            '\u{fff9}',
            '\u{fffb}',
            '\u{e0001}',
            '\u{e007f}',
        ] {
            let s = format!("a{c}b");
            let out = printable(&s);
            assert!(!out.contains(c), "{c:?} kept: {out:?}");
        }
    }
}
