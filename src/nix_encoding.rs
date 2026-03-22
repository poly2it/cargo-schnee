//! Shared Nix encoding utilities: base32, hex, hash compression, store path parsing.

/// Nix base32 alphabet (no e, o, t, u).
pub(crate) const NIX_BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

pub(crate) fn nix_base32_encode(input: &[u8]) -> String {
    let len = (input.len() * 8).div_ceil(5);
    let mut out = vec![0u8; len];
    for n in 0..len {
        let mut b: u8 = 0;
        for bit in 0..5 {
            let pos = n * 5 + bit;
            if pos / 8 < input.len() {
                b |= ((input[pos / 8] >> (pos % 8)) & 1) << bit;
            }
        }
        out[len - 1 - n] = NIX_BASE32[b as usize];
    }
    String::from_utf8(out).expect("nix_base32 output is always valid ASCII")
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// XOR-fold a hash to a shorter length.
pub(crate) fn compress_hash(hash: &[u8], target_len: usize) -> Vec<u8> {
    let mut result = vec![0u8; target_len];
    for (i, &byte) in hash.iter().enumerate() {
        result[i % target_len] ^= byte;
    }
    result
}

/// Extract the 32-char hash part from a Nix store path.
pub(crate) fn extract_hash_part(store_path: &str) -> anyhow::Result<&str> {
    let after_prefix = store_path.strip_prefix("/nix/store/").unwrap_or(store_path);
    anyhow::ensure!(
        after_prefix.len() >= 32,
        "store path hash too short ({} chars, need 32): {}",
        after_prefix.len(),
        store_path
    );
    Ok(&after_prefix[..32])
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn nix_base32_encode_empty() {
        assert_eq!(nix_base32_encode(&[]), "");
    }

    #[test]
    fn nix_base32_encode_known_digest() {
        // SHA-256 of empty string
        let digest = Sha256::digest(b"");
        let encoded = nix_base32_encode(&digest);
        // 256 bits → ceil(256/5) = 52 chars
        assert_eq!(encoded.len(), 52);
        // All chars must be from the nix base32 alphabet
        assert!(encoded.bytes().all(|b| NIX_BASE32.contains(&b)));
    }

    #[test]
    fn nix_base32_encode_deterministic() {
        let input = b"hello world";
        assert_eq!(nix_base32_encode(input), nix_base32_encode(input));
    }

    #[test]
    fn compress_hash_xor_fold() {
        // When hash length == target, result is identity
        let hash = vec![0xAB, 0xCD, 0xEF];
        assert_eq!(compress_hash(&hash, 3), hash);

        // When hash is exactly 2x target, result is XOR of halves
        let hash = vec![0xFF, 0x00, 0xAA, 0x55];
        let compressed = compress_hash(&hash, 2);
        assert_eq!(compressed, vec![0xFF ^ 0xAA, 0x55]);
    }

    #[test]
    fn compress_hash_empty() {
        assert_eq!(compress_hash(&[], 4), vec![0, 0, 0, 0]);
    }

    #[test]
    fn hex_lower_basic() {
        assert_eq!(hex_lower(&[0x00, 0xFF, 0xAB]), "00ffab");
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn extract_hash_part_valid() {
        let path = "/nix/store/abcdefghijklmnopqrstuvwxyz012345-foo";
        assert_eq!(
            extract_hash_part(path).unwrap(),
            "abcdefghijklmnopqrstuvwxyz012345"
        );
    }

    #[test]
    fn extract_hash_part_no_prefix() {
        let path = "abcdefghijklmnopqrstuvwxyz012345-foo";
        assert_eq!(
            extract_hash_part(path).unwrap(),
            "abcdefghijklmnopqrstuvwxyz012345"
        );
    }

    #[test]
    fn extract_hash_part_too_short() {
        assert!(extract_hash_part("/nix/store/abc").is_err());
    }

    #[test]
    fn extract_hash_part_empty() {
        assert!(extract_hash_part("").is_err());
    }

    #[test]
    fn extract_hash_part_exactly_32() {
        let path = "/nix/store/abcdefghijklmnopqrstuvwxyz012345";
        assert_eq!(
            extract_hash_part(path).unwrap(),
            "abcdefghijklmnopqrstuvwxyz012345"
        );
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_nix_base32_length(input in proptest::collection::vec(any::<u8>(), 0..256)) {
            let expected_len = (input.len() * 8).div_ceil(5);
            prop_assert_eq!(nix_base32_encode(&input).len(), expected_len);
        }

        #[test]
        fn prop_nix_base32_alphabet(input in proptest::collection::vec(any::<u8>(), 0..256)) {
            let encoded = nix_base32_encode(&input);
            for b in encoded.bytes() {
                prop_assert!(NIX_BASE32.contains(&b), "char {} not in alphabet", b as char);
            }
        }

        #[test]
        fn prop_nix_base32_deterministic(input in proptest::collection::vec(any::<u8>(), 0..256)) {
            prop_assert_eq!(nix_base32_encode(&input), nix_base32_encode(&input));
        }

        #[test]
        fn prop_compress_hash_length(
            hash in proptest::collection::vec(any::<u8>(), 0..256),
            target_len in 1..64usize,
        ) {
            prop_assert_eq!(compress_hash(&hash, target_len).len(), target_len);
        }

        #[test]
        fn prop_compress_hash_identity(hash in proptest::collection::vec(any::<u8>(), 1..64)) {
            prop_assert_eq!(compress_hash(&hash, hash.len()), hash);
        }

        #[test]
        fn prop_extract_hash_part_length(
            hash_chars in "[0-9a-z]{32}",
            name in "[a-z][a-z0-9-]{0,30}",
        ) {
            let path = format!("/nix/store/{}-{}", hash_chars, name);
            let result = extract_hash_part(&path).unwrap();
            prop_assert_eq!(result.len(), 32);
            prop_assert_eq!(result, &hash_chars[..]);
        }
    }
}
