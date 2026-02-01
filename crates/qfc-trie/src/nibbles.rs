//! Nibble operations for trie keys
//!
//! Nibbles are 4-bit values (0-15) used as path elements in the trie.

/// A slice of nibbles
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NibbleSlice {
    /// The underlying bytes
    data: Vec<u8>,
    /// Start nibble index
    start: usize,
    /// Number of nibbles
    len: usize,
}

impl NibbleSlice {
    /// Create from bytes (each byte becomes 2 nibbles)
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            data: bytes.to_vec(),
            start: 0,
            len: bytes.len() * 2,
        }
    }

    /// Create from nibbles directly
    pub fn from_nibbles(nibbles: &[u8]) -> Self {
        // Pack nibbles into bytes
        let mut data = Vec::with_capacity((nibbles.len() + 1) / 2);
        for chunk in nibbles.chunks(2) {
            let byte = if chunk.len() == 2 {
                (chunk[0] << 4) | (chunk[1] & 0x0f)
            } else {
                chunk[0] << 4
            };
            data.push(byte);
        }

        Self {
            data,
            start: 0,
            len: nibbles.len(),
        }
    }

    /// Get the length in nibbles
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get nibble at index
    pub fn at(&self, index: usize) -> u8 {
        assert!(index < self.len);
        let actual_index = self.start + index;
        let byte = self.data[actual_index / 2];
        if actual_index % 2 == 0 {
            (byte >> 4) & 0x0f
        } else {
            byte & 0x0f
        }
    }

    /// Get a slice starting from offset
    pub fn offset(&self, offset: usize) -> Self {
        assert!(offset <= self.len);
        Self {
            data: self.data.clone(),
            start: self.start + offset,
            len: self.len - offset,
        }
    }

    /// Get a slice of the first `count` nibbles
    pub fn prefix(&self, count: usize) -> Self {
        assert!(count <= self.len);
        Self {
            data: self.data.clone(),
            start: self.start,
            len: count,
        }
    }

    /// Find common prefix length with another nibble slice
    pub fn common_prefix_len(&self, other: &NibbleSlice) -> usize {
        let max_len = std::cmp::min(self.len, other.len);
        for i in 0..max_len {
            if self.at(i) != other.at(i) {
                return i;
            }
        }
        max_len
    }

    /// Check if this starts with the other slice
    pub fn starts_with(&self, prefix: &NibbleSlice) -> bool {
        if prefix.len > self.len {
            return false;
        }
        for i in 0..prefix.len {
            if self.at(i) != prefix.at(i) {
                return false;
            }
        }
        true
    }

    /// Convert to nibble vector
    pub fn to_nibbles(&self) -> Vec<u8> {
        (0..self.len).map(|i| self.at(i)).collect()
    }

    /// Convert to bytes (must be even length)
    pub fn to_bytes(&self) -> Vec<u8> {
        assert!(self.len % 2 == 0, "nibble length must be even for to_bytes");
        let mut result = Vec::with_capacity(self.len / 2);
        for i in (0..self.len).step_by(2) {
            let byte = (self.at(i) << 4) | self.at(i + 1);
            result.push(byte);
        }
        result
    }

    /// Encode for storage with flag for leaf/extension
    pub fn encode_compact(&self, is_leaf: bool) -> Vec<u8> {
        let odd = self.len % 2 == 1;
        let flag = if is_leaf { 2 } else { 0 } + if odd { 1 } else { 0 };

        let mut result = Vec::with_capacity((self.len + 2) / 2);

        if odd {
            // Odd length: first byte is flag nibble + first nibble
            result.push((flag << 4) | self.at(0));
            for i in (1..self.len).step_by(2) {
                if i + 1 < self.len {
                    result.push((self.at(i) << 4) | self.at(i + 1));
                }
            }
        } else {
            // Even length: first byte is flag nibble + 0
            result.push(flag << 4);
            for i in (0..self.len).step_by(2) {
                result.push((self.at(i) << 4) | self.at(i + 1));
            }
        }

        result
    }

    /// Decode from compact encoding
    pub fn decode_compact(data: &[u8]) -> Option<(Self, bool)> {
        if data.is_empty() {
            return None;
        }

        let flag = (data[0] >> 4) & 0x0f;
        let is_leaf = flag & 2 != 0;
        let odd = flag & 1 != 0;

        let mut nibbles = Vec::new();

        if odd {
            // First nibble is in first byte
            nibbles.push(data[0] & 0x0f);
        }

        for &byte in &data[1..] {
            nibbles.push((byte >> 4) & 0x0f);
            nibbles.push(byte & 0x0f);
        }

        Some((Self::from_nibbles(&nibbles), is_leaf))
    }
}

impl std::fmt::Display for NibbleSlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[")?;
        for i in 0..self.len {
            write!(f, "{:x}", self.at(i))?;
        }
        write!(f, "]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_bytes() {
        let nibbles = NibbleSlice::from_bytes(&[0x12, 0x34]);
        assert_eq!(nibbles.len(), 4);
        assert_eq!(nibbles.at(0), 1);
        assert_eq!(nibbles.at(1), 2);
        assert_eq!(nibbles.at(2), 3);
        assert_eq!(nibbles.at(3), 4);
    }

    #[test]
    fn test_offset() {
        let nibbles = NibbleSlice::from_bytes(&[0x12, 0x34]);
        let offset = nibbles.offset(2);
        assert_eq!(offset.len(), 2);
        assert_eq!(offset.at(0), 3);
        assert_eq!(offset.at(1), 4);
    }

    #[test]
    fn test_common_prefix() {
        let a = NibbleSlice::from_bytes(&[0x12, 0x34]);
        let b = NibbleSlice::from_bytes(&[0x12, 0x56]);
        assert_eq!(a.common_prefix_len(&b), 2);

        let c = NibbleSlice::from_bytes(&[0xab, 0xcd]);
        assert_eq!(a.common_prefix_len(&c), 0);
    }

    #[test]
    fn test_compact_encoding_even() {
        let nibbles = NibbleSlice::from_nibbles(&[1, 2, 3, 4]);
        let encoded = nibbles.encode_compact(false);
        let (decoded, is_leaf) = NibbleSlice::decode_compact(&encoded).unwrap();
        assert!(!is_leaf);
        assert_eq!(decoded.to_nibbles(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_compact_encoding_odd() {
        let nibbles = NibbleSlice::from_nibbles(&[1, 2, 3]);
        let encoded = nibbles.encode_compact(true);
        let (decoded, is_leaf) = NibbleSlice::decode_compact(&encoded).unwrap();
        assert!(is_leaf);
        assert_eq!(decoded.to_nibbles(), vec![1, 2, 3]);
    }
}
