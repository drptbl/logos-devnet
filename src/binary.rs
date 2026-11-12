//! RISC Zero program binaries (`.bin`, the `R0BF` format) as LEZ's program loader splits them.
//!
//! ```text
//! "R0BF"  format version (u32)  header length (u32)  header  user ELF length (u32)  user ELF  kernel ELF
//! ```
//!
//! All numbers are little-endian. The loader stores only the user ELF, in segments of
//! `SEGMENT_LEN` bytes, and refuses a deploy whose segment accounts don't number exactly that.

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

/// LEZ's `MAX_SEGMENT_DATA_LEN`.
const SEGMENT_LEN: usize = 96 * 1024;

const MAGIC: &[u8] = b"R0BF";
const FORMAT_VERSION: u32 = 1;

/// `bytes`' SHA-256, in lower-case hex, as `shasum -a 256` prints it.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// How many segment accounts the loader needs for `binary`.
pub fn segments(binary: &[u8]) -> Result<usize> {
    Ok(user_elf_len(binary)?.div_ceil(SEGMENT_LEN))
}

fn user_elf_len(binary: &[u8]) -> Result<usize> {
    let Some(rest) = binary.strip_prefix(MAGIC) else {
        bail!(
            "not a RISC Zero program binary: deploy the guest's `.bin` (from `cargo risczero \
             build`), not its ELF"
        );
    };
    let (version, rest) = read_u32(rest)?;
    if version != FORMAT_VERSION {
        bail!("a RISC Zero program binary of format {version}, which LEZ doesn't load");
    }
    let (header_len, rest) = read_u32(rest)?;
    let rest = rest.get(header_len as usize..).ok_or_else(truncated)?;
    let (user_len, rest) = read_u32(rest)?;
    let user_len = user_len as usize;
    // The kernel ELF follows, and must not be empty.
    if user_len == 0 || rest.len() <= user_len {
        return Err(truncated());
    }
    Ok(user_len)
}

fn read_u32(bytes: &[u8]) -> Result<(u32, &[u8])> {
    let (number, rest) = bytes.split_first_chunk::<4>().ok_or_else(truncated)?;
    Ok((u32::from_le_bytes(*number), rest))
}

fn truncated() -> anyhow::Error {
    anyhow::anyhow!("a truncated RISC Zero program binary")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The kernel every LEZ program is built with, in bytes.
    const KERNEL_LEN: usize = 32_392;

    /// A `.bin` with a user ELF of `user_len` bytes.
    pub(crate) fn program_binary(user_len: usize) -> Vec<u8> {
        let header = [7u8; 16];
        let mut bin = MAGIC.to_vec();
        bin.extend(FORMAT_VERSION.to_le_bytes());
        bin.extend((header.len() as u32).to_le_bytes());
        bin.extend(header);
        bin.extend((user_len as u32).to_le_bytes());
        bin.extend(vec![1u8; user_len]);
        bin.extend(vec![2u8; KERNEL_LEN]);
        bin
    }

    #[test]
    fn counts_the_user_elf_only() {
        // The kernel takes this file past one segment, the user ELF doesn't.
        let bin = program_binary(SEGMENT_LEN - 100);
        assert!(bin.len() > SEGMENT_LEN);
        assert_eq!(segments(&bin).unwrap(), 1);
        assert_eq!(segments(&program_binary(SEGMENT_LEN)).unwrap(), 1);
        assert_eq!(segments(&program_binary(SEGMENT_LEN + 1)).unwrap(), 2);
        // git_logos_registry.bin: 219,664 bytes, 187,240 of them the user ELF.
        assert_eq!(segments(&program_binary(187_240)).unwrap(), 2);
    }

    #[test]
    fn refuses_an_elf() {
        let err = segments(b"\x7fELF\x01\x01\x01\0rest").unwrap_err();
        assert!(err.to_string().contains("not its ELF"), "{err}");
    }

    #[test]
    fn refuses_truncated_and_unknown_binaries() {
        let bin = program_binary(1000);
        for cut in [4, 10, 30, 40, bin.len() - KERNEL_LEN] {
            assert!(segments(&bin[..cut]).is_err(), "cut at {cut}");
        }
        let mut other = bin.clone();
        other[4] = 2;
        assert!(
            segments(&other)
                .unwrap_err()
                .to_string()
                .contains("format 2")
        );
    }
}
