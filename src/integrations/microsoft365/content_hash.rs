//! Content hashes of an uploaded file, to prove that a file found in OneDrive
//! is the very bytes that were picked.
//!
//! Graph's [`hashes` resource](https://learn.microsoft.com/en-us/graph/api/resources/hashes)
//! says `quickXorHash` is the one value guaranteed for OneDrive for work or
//! school, and that `sha256Hash` "isn't supported". So the file is hashed with
//! the documented
//! [QuickXorHash](https://learn.microsoft.com/en-us/onedrive/developer/code-snippets/quickxorhash)
//! (160 bits, base64) and with SHA-256 (hex), and a found file is compared on
//! whichever of the two Graph reports. `sha1Hash` and `crc32Hash` are ignored:
//! a weak hash must never vouch for a file. When Graph reports neither
//! comparable hash, nothing is proven.

use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::Read;

const WIDTH_IN_BITS: usize = 160;
const SHIFT: usize = 11;
const BITS_IN_LAST_CELL: usize = 32;

/// The documented QuickXorHash, ported from Microsoft's reference code.
pub(super) struct QuickXorHash {
    data: [u64; 3],
    length_so_far: u64,
    shift_so_far: usize,
}

impl QuickXorHash {
    pub(super) const fn new() -> Self {
        Self {
            data: [0; 3],
            length_so_far: 0,
            shift_so_far: 0,
        }
    }

    pub(super) fn update(&mut self, array: &[u8]) {
        let size = array.len();
        let mut vector_index = self.shift_so_far / 64;
        let mut vector_offset = self.shift_so_far % 64;
        let iterations = size.min(WIDTH_IN_BITS);
        for i in 0..iterations {
            let is_last_cell = vector_index == self.data.len() - 1;
            let bits_in_cell = if is_last_cell { BITS_IN_LAST_CELL } else { 64 };
            if vector_offset <= bits_in_cell - 8 {
                let mut j = i;
                while j < size {
                    self.data[vector_index] ^= u64::from(array[j]) << vector_offset;
                    j += WIDTH_IN_BITS;
                }
            } else {
                let index1 = vector_index;
                let index2 = if is_last_cell { 0 } else { vector_index + 1 };
                let low = bits_in_cell - vector_offset;
                let mut xored = 0_u8;
                let mut j = i;
                while j < size {
                    xored ^= array[j];
                    j += WIDTH_IN_BITS;
                }
                self.data[index1] ^= u64::from(xored) << vector_offset;
                self.data[index2] ^= u64::from(xored) >> low;
            }
            vector_offset += SHIFT;
            while vector_offset >= bits_in_cell {
                vector_index = if is_last_cell { 0 } else { vector_index + 1 };
                vector_offset -= bits_in_cell;
            }
        }
        self.shift_so_far = (self.shift_so_far + SHIFT * (size % WIDTH_IN_BITS)) % WIDTH_IN_BITS;
        self.length_so_far += size as u64;
    }

    /// The 20-byte digest as base64, the form Graph reports.
    pub(super) fn finish(self) -> String {
        let mut rgb = [0_u8; WIDTH_IN_BITS / 8];
        rgb[0..8].copy_from_slice(&self.data[0].to_le_bytes());
        rgb[8..16].copy_from_slice(&self.data[1].to_le_bytes());
        rgb[16..20].copy_from_slice(&self.data[2].to_le_bytes()[0..4]);
        for (index, byte) in self.length_so_far.to_le_bytes().iter().enumerate() {
            rgb[12 + index] ^= byte;
        }
        base64::engine::general_purpose::STANDARD.encode(rgb)
    }
}

/// Both comparable hashes of one file.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct ContentHashes {
    pub(super) quick_xor: String,
    pub(super) sha256: String,
}

impl std::fmt::Debug for ContentHashes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContentHashes")
            .finish_non_exhaustive()
    }
}

impl ContentHashes {
    pub(super) fn of_bytes(bytes: &[u8]) -> Self {
        let mut quick = QuickXorHash::new();
        quick.update(bytes);
        Self {
            quick_xor: quick.finish(),
            sha256: hex(&Sha256::digest(bytes)),
        }
    }

    /// Hashes a reader without holding it in memory.
    pub(super) fn of_reader(mut reader: impl Read) -> std::io::Result<Self> {
        let mut quick = QuickXorHash::new();
        let mut sha = Sha256::new();
        let mut buffer = vec![0_u8; 256 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            quick.update(&buffer[..read]);
            sha.update(&buffer[..read]);
        }
        Ok(Self {
            quick_xor: quick.finish(),
            sha256: hex(&sha.finalize()),
        })
    }
}

/// What Graph reported for a file.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct ReportedHashes {
    pub(super) quick_xor: Option<String>,
    pub(super) sha256: Option<String>,
}

impl std::fmt::Debug for ReportedHashes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReportedHashes")
            .finish_non_exhaustive()
    }
}

/// The result of comparing what Graph reports with the local bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HashVerdict {
    /// At least one comparable hash is reported and every reported one is
    /// exactly equal.
    Match,
    /// A reported hash differs.
    Differs,
    /// Graph reported no comparable hash.
    NotReported,
}

impl ReportedHashes {
    pub(super) fn compare(&self, local: &ContentHashes) -> HashVerdict {
        let quick = self
            .quick_xor
            .as_deref()
            .map(|value| value == local.quick_xor);
        // Hex digests are compared case-insensitively; base64 exactly.
        let sha = self
            .sha256
            .as_deref()
            .map(|value| value.eq_ignore_ascii_case(&local.sha256));
        match (quick, sha) {
            (None, None) => HashVerdict::NotReported,
            (Some(false), _) | (_, Some(false)) => HashVerdict::Differs,
            _ => HashVerdict::Match,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The algorithm as the documentation describes it in prose: a 160-bit
    /// block, each byte rotated left by `11 * i` bits and XORed in, the block
    /// byte-reversed, and the length XORed into its low 64 bits in
    /// little-endian order. An independent check of the streaming port.
    fn reference(input: &[u8]) -> String {
        let mut bits = [false; WIDTH_IN_BITS];
        for (index, byte) in input.iter().enumerate() {
            let shift = (index * SHIFT) % WIDTH_IN_BITS;
            for bit in 0..8 {
                if byte >> bit & 1 == 1 {
                    let position = (bit + shift) % WIDTH_IN_BITS;
                    bits[position] ^= true;
                }
            }
        }
        // Bit `p` of the little-endian block is bit `p % 8` of byte `p / 8`.
        let mut block = [0_u8; WIDTH_IN_BITS / 8];
        for (position, set) in bits.iter().enumerate() {
            if *set {
                block[position / 8] |= 1 << (position % 8);
            }
        }
        for (index, byte) in (input.len() as u64).to_le_bytes().iter().enumerate() {
            block[12 + index] ^= byte;
        }
        base64::engine::general_purpose::STANDARD.encode(block)
    }

    fn quick(input: &[u8]) -> String {
        let mut hash = QuickXorHash::new();
        hash.update(input);
        hash.finish()
    }

    #[test]
    fn the_empty_file_hashes_to_all_zero() {
        assert_eq!(quick(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert_eq!(reference(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn the_streaming_port_matches_the_described_algorithm_on_many_lengths() {
        let mut state = 0x1234_5678_u32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        for length in [
            1, 2, 3, 7, 8, 19, 20, 21, 63, 64, 65, 100, 159, 160, 161, 319, 320, 321, 1000, 4097,
        ] {
            let input: Vec<u8> = (0..length).map(|_| next()).collect();
            assert_eq!(quick(&input), reference(&input), "length {length}");
        }
    }

    #[test]
    fn chunking_never_changes_the_hash() {
        let input: Vec<u8> = (0..5000_u32).map(|n| (n * 31 % 251) as u8).collect();
        let whole = quick(&input);
        for chunk in [1, 7, 160, 161, 1000, 4999] {
            let mut hash = QuickXorHash::new();
            for piece in input.chunks(chunk) {
                hash.update(piece);
            }
            assert_eq!(hash.finish(), whole, "chunk {chunk}");
        }
        let streamed = ContentHashes::of_reader(std::io::Cursor::new(&input)).unwrap();
        assert_eq!(streamed, ContentHashes::of_bytes(&input));
    }

    #[test]
    fn sha256_is_the_standard_digest() {
        assert_eq!(
            ContentHashes::of_bytes(b"abc").sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_comparison_needs_a_reported_hash_and_every_reported_hash_must_match() {
        let local = ContentHashes::of_bytes(b"the bytes");
        let other = ContentHashes::of_bytes(b"other bytes");
        let reported = |quick: Option<&str>, sha: Option<&str>| ReportedHashes {
            quick_xor: quick.map(str::to_owned),
            sha256: sha.map(str::to_owned),
        };
        assert_eq!(
            reported(None, None).compare(&local),
            HashVerdict::NotReported
        );
        assert_eq!(
            reported(Some(&local.quick_xor), None).compare(&local),
            HashVerdict::Match
        );
        assert_eq!(
            reported(None, Some(&local.sha256.to_ascii_uppercase())).compare(&local),
            HashVerdict::Match
        );
        assert_eq!(
            reported(Some(&local.quick_xor), Some(&local.sha256)).compare(&local),
            HashVerdict::Match
        );
        assert_eq!(
            reported(Some(&other.quick_xor), None).compare(&local),
            HashVerdict::Differs
        );
        // One matching hash never outweighs a differing one.
        assert_eq!(
            reported(Some(&local.quick_xor), Some(&other.sha256)).compare(&local),
            HashVerdict::Differs
        );
        assert_eq!(
            reported(Some("not even base64"), None).compare(&local),
            HashVerdict::Differs
        );
    }

    #[test]
    fn hashes_never_print() {
        let local = ContentHashes::of_bytes(b"x");
        assert!(!format!("{local:?}").contains(&local.sha256));
    }
}
