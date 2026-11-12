//! Program binaries the project file names by URL, e.g. a program's GitHub release: downloaded
//! once into the home with `curl`, and kept only when their SHA-256 is the one the file names.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::binary::sha256_hex;

/// A SHA-256 as the project file gives it: 64 lower-case hex digits.
pub fn check_sha256(sha256: &str) -> Result<()> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("sha256 must be 64 lower-case hex digits, as `shasum -a 256` prints it");
    }
    Ok(())
}

/// A URL as the project file gives it: http or https, so curl reads it as neither an option
/// nor a local file.
pub fn check_url(url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("url must start with https:// or http://, not {url}");
    }
    Ok(())
}

/// The file at `url` whose SHA-256 is `sha256`, kept in `dir`: downloaded the first time, and
/// from then on read from `dir`.
pub fn cached(dir: &Path, url: &str, sha256: &str) -> Result<PathBuf> {
    let path = dir.join(format!("{sha256}.bin"));
    if std::fs::read(&path).is_ok_and(|bytes| sha256_hex(&bytes) == sha256) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let part = dir.join(format!("{sha256}.part"));
    eprintln!("Downloading {url}...");
    let status = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "3",
            "--connect-timeout",
            "15",
            "--max-time",
            "600",
        ])
        .args([
            "--globoff",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "-o",
        ])
        .arg(&part)
        .arg(url)
        .stdin(Stdio::null())
        .status()
        .context("downloading program binaries needs curl")?;
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        bail!("could not download {url} (curl {status})");
    }
    let got = std::fs::read(&part).map(|bytes| sha256_hex(&bytes));
    if got.as_deref().ok() != Some(sha256) {
        let _ = std::fs::remove_file(&part);
        let got = got.with_context(|| format!("reading {}", part.display()))?;
        bail!("{url} has SHA-256 {got}, not the {sha256} the project file names");
    }
    std::fs::rename(&part, &path).with_context(|| format!("keeping {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serves `body` to each of `requests` requests on 127.0.0.1, with `status`.
    pub(crate) fn serve(status: &'static str, body: &'static [u8], requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/program.bin", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        url
    }

    #[test]
    fn downloads_once_and_keeps_what_matches() {
        let dir = tempfile::tempdir().unwrap();
        let body: &[u8] = b"a program";
        let sha = sha256_hex(body);
        // One request only: the second call must come from the cache.
        let url = serve("200 OK", body, 1);
        let path = cached(dir.path(), &url, &sha).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(cached(dir.path(), &url, &sha).unwrap(), path);
    }

    #[test]
    fn refuses_a_download_with_another_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve("200 OK", b"something else", 1);
        let err = cached(dir.path(), &url, &sha256_hex(b"a program")).unwrap_err();
        assert!(err.to_string().contains("has SHA-256"), "{err}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "nothing kept"
        );
    }

    #[test]
    fn refuses_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve("404 Not Found", b"", 4);
        assert!(cached(dir.path(), &url, &sha256_hex(b"x")).is_err());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "nothing kept"
        );
    }

    #[test]
    fn downloads_again_over_a_damaged_copy() {
        let dir = tempfile::tempdir().unwrap();
        let body: &[u8] = b"a program";
        let sha = sha256_hex(body);
        std::fs::write(dir.path().join(format!("{sha}.bin")), b"damaged").unwrap();
        let path = cached(dir.path(), &serve("200 OK", body, 1), &sha).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), body);
    }

    #[test]
    fn takes_a_sha256_as_shasum_prints_it() {
        assert!(check_sha256(&sha256_hex(b"x")).is_ok());
        assert!(check_sha256(&sha256_hex(b"x").to_uppercase()).is_err());
        assert!(check_sha256("abc").is_err());
    }
}
