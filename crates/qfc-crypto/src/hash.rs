//! Blake3 hashing functions

use qfc_types::Hash;

/// Hash data using Blake3
pub fn blake3_hash(data: &[u8]) -> Hash {
    let hash = blake3::hash(data);
    Hash::new(*hash.as_bytes())
}

/// Hash multiple data items using Blake3
pub fn blake3_hash_many(items: &[&[u8]]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    for item in items {
        hasher.update(item);
    }
    Hash::new(*hasher.finalize().as_bytes())
}

/// Compute Merkle root from a list of hashes
pub fn merkle_root(hashes: &[Hash]) -> Hash {
    if hashes.is_empty() {
        return Hash::ZERO;
    }
    if hashes.len() == 1 {
        return hashes[0];
    }

    let mut current_level: Vec<Hash> = hashes.to_vec();

    while current_level.len() > 1 {
        let mut next_level = Vec::with_capacity((current_level.len() + 1) / 2);

        for chunk in current_level.chunks(2) {
            if chunk.len() == 2 {
                next_level.push(blake3_hash_many(&[
                    chunk[0].as_bytes(),
                    chunk[1].as_bytes(),
                ]));
            } else {
                // Odd number of elements: hash the last one with itself
                next_level.push(blake3_hash_many(&[
                    chunk[0].as_bytes(),
                    chunk[0].as_bytes(),
                ]));
            }
        }

        current_level = next_level;
    }

    current_level[0]
}

/// Compute hash of a transaction for signing
pub fn transaction_hash(tx_bytes: &[u8]) -> Hash {
    blake3_hash(tx_bytes)
}

/// Compute hash of a block header
pub fn block_hash(header_bytes: &[u8]) -> Hash {
    blake3_hash(header_bytes)
}

/// Compute hash with domain separation
pub fn domain_hash(domain: &str, data: &[u8]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0u8]); // Separator
    hasher.update(data);
    Hash::new(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blake3_hash() {
        let data = b"hello world";
        let hash1 = blake3_hash(data);
        let hash2 = blake3_hash(data);
        assert_eq!(hash1, hash2);

        let different_hash = blake3_hash(b"different data");
        assert_ne!(hash1, different_hash);
    }

    #[test]
    fn test_blake3_hash_many() {
        let data1 = b"hello";
        let data2 = b"world";
        let combined = blake3_hash_many(&[data1, data2]);

        // Sequential updates are equivalent to hashing concatenated data
        let concat_hash = blake3_hash(b"helloworld");
        assert_eq!(combined, concat_hash);

        // But different from hashing each separately then combining
        let hash1 = blake3_hash(data1);
        let hash2 = blake3_hash(data2);
        let separate_combined = blake3_hash_many(&[hash1.as_bytes(), hash2.as_bytes()]);
        assert_ne!(combined, separate_combined);
    }

    #[test]
    fn test_merkle_root_empty() {
        let root = merkle_root(&[]);
        assert_eq!(root, Hash::ZERO);
    }

    #[test]
    fn test_merkle_root_single() {
        let hash = blake3_hash(b"test");
        let root = merkle_root(&[hash]);
        assert_eq!(root, hash);
    }

    #[test]
    fn test_merkle_root_multiple() {
        let h1 = blake3_hash(b"a");
        let h2 = blake3_hash(b"b");
        let h3 = blake3_hash(b"c");

        let root = merkle_root(&[h1, h2, h3]);
        assert_ne!(root, Hash::ZERO);

        // Verify deterministic
        let root2 = merkle_root(&[h1, h2, h3]);
        assert_eq!(root, root2);

        // Different order should give different root
        let root3 = merkle_root(&[h2, h1, h3]);
        assert_ne!(root, root3);
    }

    #[test]
    fn test_domain_hash() {
        let hash1 = domain_hash("tx", b"data");
        let hash2 = domain_hash("block", b"data");
        assert_ne!(hash1, hash2);
    }
}
