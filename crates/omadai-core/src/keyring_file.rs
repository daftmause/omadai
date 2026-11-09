//! Detects gnome-keyring's plain-text storage format for the default keyring.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyringEncryption {
    Encrypted,
    /// The default keyring has no password, so its file holds every secret in plain text.
    Unencrypted(PathBuf),
    /// No gnome-keyring file to inspect, as with another Secret Service provider.
    Unknown(String),
}

const PLAIN_HEADER: &[u8] = b"[keyring]";
const ENCRYPTED_HEADER: &[u8] = b"GnomeKeyring\n\r\0\n";

/// Reads only the first bytes of `<keyrings_dir>/<default>.keyring`, where the `default` file names the keyring.
pub fn detect(keyrings_dir: &Path) -> KeyringEncryption {
    let name = match std::fs::read_to_string(keyrings_dir.join("default")) {
        Ok(s) => s.trim().to_string(),
        Err(_) => "login".to_string(),
    };
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return KeyringEncryption::Unknown(format!(
            "unexpected default keyring name in {}",
            keyrings_dir.join("default").display()
        ));
    }
    let path = keyrings_dir.join(format!("{name}.keyring"));
    let mut head = [0u8; 16];
    let n = match File::open(&path).and_then(|mut f| read_up_to(&mut f, &mut head)) {
        Ok(n) => n,
        Err(_) => {
            return KeyringEncryption::Unknown(format!(
                "no gnome-keyring file at {}",
                path.display()
            ));
        }
    };
    let head = &head[..n];
    if head.starts_with(PLAIN_HEADER) {
        KeyringEncryption::Unencrypted(path)
    } else if head.starts_with(ENCRYPTED_HEADER) {
        KeyringEncryption::Encrypted
    } else {
        KeyringEncryption::Unknown(format!("unrecognised format in {}", path.display()))
    }
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TempDir;

    #[test]
    fn detects_each_format() {
        let t = TempDir::new("keyring");
        let dir = t.path();
        assert!(matches!(detect(dir), KeyringEncryption::Unknown(_)));
        std::fs::write(dir.join("default"), "login\n").unwrap();
        std::fs::write(dir.join("login.keyring"), "[keyring]\ndisplay-name=Login\n").unwrap();
        assert_eq!(
            detect(dir),
            KeyringEncryption::Unencrypted(dir.join("login.keyring"))
        );
        std::fs::write(dir.join("login.keyring"), b"GnomeKeyring\n\r\0\n\0\0binary").unwrap();
        assert_eq!(detect(dir), KeyringEncryption::Encrypted);
        std::fs::write(dir.join("default"), "work").unwrap();
        std::fs::write(dir.join("work.keyring"), "[keyring]\n").unwrap();
        assert!(matches!(detect(dir), KeyringEncryption::Unencrypted(_)));
        std::fs::write(dir.join("default"), "../escape").unwrap();
        assert!(matches!(detect(dir), KeyringEncryption::Unknown(_)));
    }
}
