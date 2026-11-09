use std::fmt;
use zeroize::Zeroizing;

/// A credential held in memory: never printed, never serialized, zeroed on drop.
///
/// There is no `Clone`: a copy would be one more buffer to zero, and every caller so far can
/// borrow or rebuild from the keystore instead.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Takes ownership of `value` without copying its buffer.
    pub fn new(value: String) -> Self {
        Secret(Zeroizing::new(value))
    }

    /// Builds a secret from bytes read out of a keystore; invalid UTF-8 is zeroed and rejected.
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        match String::from_utf8(bytes) {
            Ok(s) => Some(Secret::new(s)),
            Err(e) => {
                drop(Zeroizing::new(e.into_bytes()));
                None
            }
        }
    }

    /// Joins parts with `:` into one secret, sized up front so no reallocation leaves a copy behind.
    pub fn join(parts: &[&Secret]) -> Self {
        let len = parts.iter().map(|p| p.0.len()).sum::<usize>() + parts.len().saturating_sub(1);
        let mut out = String::with_capacity(len);
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                out.push(':');
            }
            out.push_str(p.expose());
        }
        Secret::new(out)
    }

    /// Prepends `prefix` (for example `"Bearer "`) into a new zeroizing buffer.
    pub fn with_prefix(prefix: &str, secret: &Secret) -> Self {
        let mut out = String::with_capacity(prefix.len() + secret.0.len());
        out.push_str(prefix);
        out.push_str(secret.expose());
        Secret::new(out)
    }

    /// The only way to read the value.
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Removes surrounding whitespace in place, without copying the value.
    pub fn trim_in_place(&mut self) {
        let end = self.0.trim_end().len();
        self.0.truncate(end);
        let start = self.0.len() - self.0.trim_start().len();
        if start > 0 {
            self.0.drain(..start);
        }
    }

    /// The `:`-separated parts, as used by `id:secret` and `AK:SK` credentials.
    pub fn parts(&self) -> impl Iterator<Item = &str> {
        self.0.split(':')
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "fk-test-0123456789abcdef";

    #[test]
    fn debug_and_display_redact() {
        let s = Secret::new(KEY.to_string());
        for text in [
            format!("{s:?}"),
            format!("{s}"),
            format!("{s:#?}"),
            format!("{:?}", Some(&s)),
        ] {
            assert!(!text.contains(KEY), "secret leaked: {text}");
            assert!(!text.contains("fk-test"), "secret prefix leaked: {text}");
            assert!(text.contains("[redacted]"));
        }
    }

    #[test]
    fn expose_returns_value() {
        assert_eq!(Secret::new(KEY.to_string()).expose(), KEY);
    }

    #[test]
    fn trim_join_prefix() {
        let mut s = Secret::new(format!("  {KEY}\r\n"));
        s.trim_in_place();
        assert_eq!(s.expose(), KEY);
        let a = Secret::new("AK".into());
        let b = Secret::new("SK".into());
        assert_eq!(Secret::join(&[&a, &b]).expose(), "AK:SK");
        assert_eq!(Secret::with_prefix("Bearer ", &a).expose(), "Bearer AK");
        assert_eq!(
            Secret::join(&[&a, &b]).parts().collect::<Vec<_>>(),
            ["AK", "SK"]
        );
    }

    #[test]
    fn invalid_utf8_rejected() {
        assert!(Secret::from_bytes(vec![0xff, 0xfe]).is_none());
        assert_eq!(Secret::from_bytes(b"abc".to_vec()).unwrap().expose(), "abc");
    }
}
