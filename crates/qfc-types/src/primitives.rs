//! Primitive types: Hash, Address, U256, Signature

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// 32-byte hash (Blake3)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, BorshSerialize, BorshDeserialize)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub const ZERO: Hash = Hash([0u8; 32]);

    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(slice: &[u8]) -> Option<Self> {
        if slice.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(slice);
        Some(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash(0x{})", hex::encode(&self.0[..8]))
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(&self.0))
    }
}

impl Serialize for Hash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", hex::encode(&self.0)))
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        Hash::from_slice(&bytes).ok_or_else(|| serde::de::Error::custom("invalid hash length"))
    }
}

impl From<[u8; 32]> for Hash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// 20-byte address
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, BorshSerialize, BorshDeserialize)]
pub struct Address(pub [u8; 20]);

impl Address {
    pub const ZERO: Address = Address([0u8; 20]);
    pub const MAX: Address = Address([0xffu8; 20]);

    pub fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(slice: &[u8]) -> Option<Self> {
        if slice.len() != 20 {
            return None;
        }
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(slice);
        Some(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address(0x{})", hex::encode(&self.0))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(&self.0))
    }
}

impl Serialize for Address {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", hex::encode(&self.0)))
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        Address::from_slice(&bytes)
            .ok_or_else(|| serde::de::Error::custom("invalid address length"))
    }
}

impl From<[u8; 20]> for Address {
    fn from(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Address {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// 256-bit unsigned integer
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct U256(pub primitive_types::U256);

impl U256 {
    pub const ZERO: U256 = U256(primitive_types::U256::zero());
    pub const ONE: U256 = U256(primitive_types::U256::one());
    pub const MAX: U256 = U256(primitive_types::U256::max_value());

    pub fn zero() -> Self {
        Self::ZERO
    }

    pub fn one() -> Self {
        Self::ONE
    }

    pub fn from_u64(val: u64) -> Self {
        Self(primitive_types::U256::from(val))
    }

    pub fn from_u128(val: u128) -> Self {
        Self(primitive_types::U256::from(val))
    }

    pub fn from_be_bytes(bytes: &[u8; 32]) -> Self {
        Self(primitive_types::U256::from_big_endian(bytes))
    }

    pub fn from_le_bytes(bytes: &[u8; 32]) -> Self {
        Self(primitive_types::U256::from_little_endian(bytes))
    }

    pub fn to_be_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        self.0.to_big_endian(&mut bytes);
        bytes
    }

    pub fn to_le_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        self.0.to_little_endian(&mut bytes);
        bytes
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    pub fn checked_add(&self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    pub fn checked_sub(&self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }

    pub fn checked_mul(&self, other: Self) -> Option<Self> {
        self.0.checked_mul(other.0).map(Self)
    }

    pub fn checked_div(&self, other: Self) -> Option<Self> {
        self.0.checked_div(other.0).map(Self)
    }

    pub fn saturating_add(&self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub fn saturating_sub(&self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    pub fn saturating_mul(&self, other: Self) -> Self {
        Self(self.0.saturating_mul(other.0))
    }

    pub fn low_u64(&self) -> u64 {
        self.0.low_u64()
    }

    pub fn low_u128(&self) -> u128 {
        self.0.low_u128()
    }
}

impl fmt::Debug for U256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "U256({})", self.0)
    }
}

impl fmt::Display for U256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::ops::Add for U256 {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl std::ops::Sub for U256 {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self(self.0 - other.0)
    }
}

impl std::ops::Mul for U256 {
    type Output = Self;
    fn mul(self, other: Self) -> Self {
        Self(self.0 * other.0)
    }
}

impl std::ops::Div for U256 {
    type Output = Self;
    fn div(self, other: Self) -> Self {
        Self(self.0 / other.0)
    }
}

impl std::ops::AddAssign for U256 {
    fn add_assign(&mut self, other: Self) {
        self.0 = self.0 + other.0;
    }
}

impl std::ops::SubAssign for U256 {
    fn sub_assign(&mut self, other: Self) {
        self.0 = self.0 - other.0;
    }
}

impl From<u64> for U256 {
    fn from(val: u64) -> Self {
        Self::from_u64(val)
    }
}

impl From<u128> for U256 {
    fn from(val: u128) -> Self {
        Self::from_u128(val)
    }
}

impl BorshSerialize for U256 {
    fn serialize<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        let bytes = self.to_le_bytes();
        writer.write_all(&bytes)
    }
}

impl BorshDeserialize for U256 {
    fn deserialize_reader<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        let mut bytes = [0u8; 32];
        reader.read_exact(&mut bytes)?;
        Ok(Self::from_le_bytes(&bytes))
    }
}

impl Serialize for U256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{:x}", self.0))
    }
}

impl<'de> Deserialize<'de> for U256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let inner =
            primitive_types::U256::from_str_radix(s, 16).map_err(serde::de::Error::custom)?;
        Ok(Self(inner))
    }
}

/// 64-byte Ed25519 signature
#[derive(Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Signature(pub [u8; 64]);

impl Signature {
    pub const ZERO: Signature = Signature([0u8; 64]);

    pub fn new(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(slice: &[u8]) -> Option<Self> {
        if slice.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 64];
        bytes.copy_from_slice(slice);
        Some(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl Default for Signature {
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature(0x{}...)", hex::encode(&self.0[..8]))
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(&self.0))
    }
}

impl Serialize for Signature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", hex::encode(&self.0)))
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        Signature::from_slice(&bytes)
            .ok_or_else(|| serde::de::Error::custom("invalid signature length"))
    }
}

impl From<[u8; 64]> for Signature {
    fn from(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Signature {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// 32-byte Ed25519 public key
#[derive(Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    pub const ZERO: PublicKey = PublicKey([0u8; 32]);

    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(slice: &[u8]) -> Option<Self> {
        if slice.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(slice);
        Some(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl Default for PublicKey {
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey(0x{})", hex::encode(&self.0[..8]))
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(&self.0))
    }
}

impl Serialize for PublicKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", hex::encode(&self.0)))
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        PublicKey::from_slice(&bytes)
            .ok_or_else(|| serde::de::Error::custom("invalid public key length"))
    }
}

impl From<[u8; 32]> for PublicKey {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for PublicKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_serialization() {
        let hash = Hash::new([0xab; 32]);
        let json = serde_json::to_string(&hash).unwrap();
        assert!(json.contains("0xabab"));
        let decoded: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(hash, decoded);
    }

    #[test]
    fn test_address_serialization() {
        let addr = Address::new([0x12; 20]);
        let json = serde_json::to_string(&addr).unwrap();
        let decoded: Address = serde_json::from_str(&json).unwrap();
        assert_eq!(addr, decoded);
    }

    #[test]
    fn test_u256_arithmetic() {
        let a = U256::from_u64(100);
        let b = U256::from_u64(50);
        assert_eq!(a + b, U256::from_u64(150));
        assert_eq!(a - b, U256::from_u64(50));
        assert_eq!(a * b, U256::from_u64(5000));
        assert_eq!(a / b, U256::from_u64(2));
    }

    #[test]
    fn test_u256_serialization() {
        let val = U256::from_u64(0x1234);
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, "\"0x1234\"");
        let decoded: U256 = serde_json::from_str(&json).unwrap();
        assert_eq!(val, decoded);
    }
}
