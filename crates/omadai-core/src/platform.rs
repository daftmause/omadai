//! The v1 platforms and the shape of credential each takes.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialShape {
    /// One key string.
    Single { prompt: &'static str },
    /// Two values stored joined as `first:second` (BytePlus AK and SK).
    Pair {
        first: &'static str,
        second: &'static str,
    },
    /// Obtained through Google sign-in rather than typed.
    GoogleSignIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub id: &'static str,
    pub name: &'static str,
    pub shape: CredentialShape,
}

pub const PLATFORMS: [Platform; 9] = [
    Platform {
        id: "openai",
        name: "OpenAI",
        shape: CredentialShape::Single {
            prompt: "OpenAI Admin key",
        },
    },
    Platform {
        id: "anthropic",
        name: "Anthropic",
        shape: CredentialShape::Single {
            prompt: "Anthropic Admin key",
        },
    },
    Platform {
        id: "fal",
        name: "fal.ai",
        shape: CredentialShape::Single {
            prompt: "fal.ai key (BILLING preset)",
        },
    },
    Platform {
        id: "higgsfield",
        name: "Higgsfield",
        shape: CredentialShape::Single {
            prompt: "Higgsfield API key (id:secret)",
        },
    },
    Platform {
        id: "gemini",
        name: "Gemini",
        shape: CredentialShape::GoogleSignIn,
    },
    Platform {
        id: "byteplus",
        name: "BytePlus",
        shape: CredentialShape::Pair {
            first: "BytePlus AccessKey ID",
            second: "BytePlus Secret Access Key",
        },
    },
    Platform {
        id: "flux",
        name: "Flux",
        shape: CredentialShape::Single {
            prompt: "BFL API key",
        },
    },
    Platform {
        id: "fish",
        name: "Fish Audio",
        shape: CredentialShape::Single {
            prompt: "Fish Audio API key",
        },
    },
    Platform {
        id: "xai",
        name: "xAI",
        shape: CredentialShape::Single {
            prompt: "xAI management key",
        },
    },
];

pub fn find(id: &str) -> Option<&'static Platform> {
    PLATFORMS.iter().find(|p| p.id == id)
}

pub fn ids() -> String {
    PLATFORMS
        .iter()
        .map(|p| p.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A label is 1 to 64 characters of letters, digits, space, `_`, `.` or `-`.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.chars().count() <= 64
        && label.trim() == label
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_and_labels() {
        assert_eq!(find("byteplus").unwrap().name, "BytePlus");
        assert!(find("../x").is_none());
        assert!(valid_label("work key-1"));
        assert!(!valid_label(""));
        assert!(!valid_label(" lead"));
        assert!(!valid_label("a/b"));
        assert!(!valid_label("sk=\"x\"\n"));
    }
}
