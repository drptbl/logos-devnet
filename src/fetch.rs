//! Program binaries the project file names by URL, e.g. a program's GitHub release: downloaded
//! once with `curl`, and kept only when their SHA-256 is the one the file names.

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

/// How old a download in progress is when no process still writes it: curl's few tries of
/// 10 minutes each, and a package download, are done well within a day.
const STALE_PART: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Where this process downloads `name` before it takes its place: `.part-<token>-<name>`, with
/// the process id and the time in the token. Every home shares the download directory, so the
/// name is this run's own, even when a process id comes round again.
pub fn part_name(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(".part-{}x{nanos}-{name}", std::process::id())
}

/// Whether `name` is one `part_name` made. Kept files never start with `.`.
fn is_part(name: &str) -> bool {
    name.strip_prefix(".part-")
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(token, file)| {
            !token.is_empty()
                && token.bytes().all(|b| b.is_ascii_alphanumeric())
                && !file.is_empty()
        })
}

/// When `path` last changed: for a directory, the newest of it and what it holds, since writing
/// a file in it leaves the directory's own time alone.
fn last_modified(path: &Path) -> Option<std::time::SystemTime> {
    let own = std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()?;
    let inside = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().and_then(|m| m.modified()).ok());
    inside.chain([own]).max()
}

/// Deletes downloads in progress in `dir` that a crashed or killed process left behind: every
/// home shares the directory, so they would pile up. One that fails to go stays.
pub fn remove_stale_parts(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = is_part(&entry.file_name().to_string_lossy())
            && last_modified(&entry.path())
                .is_some_and(|t| t.elapsed().is_ok_and(|age| age > STALE_PART));
        if !stale {
            continue;
        }
        // A symlink is removed, never followed.
        let _ = match entry.file_type() {
            Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(entry.path()),
            _ => std::fs::remove_file(entry.path()),
        };
    }
}

/// The file at `url` whose SHA-256 is `sha256`, kept in `dir`: downloaded the first time, and
/// from then on read from `dir`. Several homes share `dir`, so a download lands under a name of
/// this process's own and is renamed into place.
pub fn cached(dir: &Path, url: &str, sha256: &str) -> Result<PathBuf> {
    let path = dir.join(format!("{sha256}.bin"));
    if std::fs::read(&path).is_ok_and(|bytes| sha256_hex(&bytes) == sha256) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    remove_stale_parts(dir);
    let part = dir.join(part_name(&format!("{sha256}.bin")));
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

    /// Reads a whole HTTP request, head and body. A server that answers and closes with some
    /// of it unread makes the OS reset the connection, and the client sees no answer.
    pub(crate) fn read_request(stream: &mut impl Read) {
        let mut seen = Vec::new();
        let mut chunk = [0u8; 4096];
        // The request's length, once its head is in: the head and a Content-Length body.
        let length = |seen: &[u8]| {
            let end = seen.windows(4).position(|w| w == b"\r\n\r\n")?;
            let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
            let body = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|n| n.trim().parse::<usize>().ok())
                .unwrap_or(0);
            Some(end + 4 + body)
        };
        while length(&seen).is_none_or(|wanted| seen.len() < wanted) {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => seen.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// Serves `body` to each of `requests` requests on 127.0.0.1, with `status`.
    pub(crate) fn serve(status: &'static str, body: &'static [u8], requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/program.bin", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                read_request(&mut stream);
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
    fn leaves_another_process_download_alone() {
        // Homes share one download directory, so another `up` may be downloading the same file.
        let dir = tempfile::tempdir().unwrap();
        let body: &[u8] = b"a program";
        let sha = sha256_hex(body);
        let theirs = dir.path().join(format!(".part-1-{sha}.bin"));
        std::fs::create_dir(&theirs).unwrap();
        let path = cached(dir.path(), &serve("200 OK", body, 1), &sha).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), body);
        assert!(theirs.is_dir(), "the other download was touched");
    }

    #[test]
    fn removes_downloads_a_crash_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600);
        let old_file = dir.path().join(format!(".part-1-{}.bin", "a".repeat(64)));
        let old_dir = dir.path().join(".part-2-lez_core-0.5.1.lgx");
        let fresh = dir.path().join(format!(".part-3-{}.bin", "b".repeat(64)));
        let kept = dir.path().join(format!("{}.bin", "c".repeat(64)));
        // A version may hold `.part-`: the package keeps its place.
        let odd = dir.path().join("lez_core-0.5.1.part-2.lgx");
        // A package still downloading: its directory is old, the file in it is not.
        let live = dir.path().join(".part-4-chat_module-0.3.0.lgx");
        std::fs::create_dir(&live).unwrap();
        std::fs::write(live.join("chat_module-0.3.0.lgx"), "downloading").unwrap();
        std::fs::write(&old_file, "half").unwrap();
        std::fs::create_dir(&old_dir).unwrap();
        std::fs::write(&fresh, "downloading").unwrap();
        std::fs::write(&kept, "a program").unwrap();
        std::fs::write(&odd, "a package").unwrap();
        for path in [&old_file, &kept, &odd] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(day_ago)
                .unwrap();
        }
        for dir in [&old_dir, &live] {
            std::fs::File::open(dir)
                .unwrap()
                .set_modified(day_ago)
                .unwrap();
        }
        remove_stale_parts(dir.path());
        assert!(live.exists(), "a download still writing");
        assert!(!old_file.exists() && !old_dir.exists());
        assert!(fresh.exists(), "another process may be downloading it");
        assert!(kept.exists() && odd.exists(), "only downloads in progress");
        // A download cleans up on its way.
        std::fs::write(&old_file, "half").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old_file)
            .unwrap()
            .set_modified(day_ago)
            .unwrap();
        let body: &[u8] = b"a program";
        cached(dir.path(), &serve("200 OK", body, 1), &sha256_hex(body)).unwrap();
        assert!(!old_file.exists());
    }

    #[test]
    fn takes_a_sha256_as_shasum_prints_it() {
        assert!(check_sha256(&sha256_hex(b"x")).is_ok());
        assert!(check_sha256(&sha256_hex(b"x").to_uppercase()).is_err());
        assert!(check_sha256("abc").is_err());
    }
}
