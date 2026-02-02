//! QVM Value types
//!
//! Defines the value types that can exist on the QVM stack and in memory.

use primitive_types::{H160, H256, U256};
use std::fmt;

/// QVM Value - represents any value in the VM
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// 256-bit unsigned integer (primary numeric type)
    U256(U256),

    /// Boolean value
    Bool(bool),

    /// 20-byte address
    Address(H160),

    /// 32-byte hash/bytes32
    Bytes32(H256),

    /// Dynamic bytes
    Bytes(Vec<u8>),

    /// UTF-8 string
    String(String),

    /// Tuple of values
    Tuple(Vec<Value>),

    /// Array of values (homogeneous)
    Array(Vec<Value>),

    /// Struct instance
    Struct {
        type_name: String,
        fields: Vec<(String, Value)>,
    },

    /// Resource value (linear type)
    Resource {
        type_name: String,
        fields: Vec<(String, Value)>,
        id: u64,
    },

    /// Reference to a value
    Ref(Box<ValueRef>),

    /// Mutable reference to a value
    RefMut(Box<ValueRef>),

    /// Unit/void value
    Unit,

    /// None/null value
    None,
}

/// Reference to a value location
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueRef {
    /// Reference to a local variable
    Local(usize),

    /// Reference to a storage slot
    Storage(H256),

    /// Reference to memory location
    Memory(usize),

    /// Reference to a struct field
    Field(Box<ValueRef>, String),

    /// Reference to an array element
    Index(Box<ValueRef>, usize),
}

impl Value {
    /// Create a U256 value from u64
    pub fn from_u64(n: u64) -> Self {
        Value::U256(U256::from(n))
    }

    /// Create a U256 value from u128
    pub fn from_u128(n: u128) -> Self {
        Value::U256(U256::from(n))
    }

    /// Create a zero U256 value
    pub fn zero() -> Self {
        Value::U256(U256::zero())
    }

    /// Create a one U256 value
    pub fn one() -> Self {
        Value::U256(U256::one())
    }

    /// Check if value is zero
    pub fn is_zero(&self) -> bool {
        match self {
            Value::U256(n) => n.is_zero(),
            Value::Bool(b) => !*b,
            Value::Unit | Value::None => true,
            _ => false,
        }
    }

    /// Check if value is truthy (for conditionals)
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::U256(n) => !n.is_zero(),
            Value::Unit | Value::None => false,
            Value::Bytes(b) => !b.is_empty(),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            _ => true,
        }
    }

    /// Get type name for error messages
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::U256(_) => "u256",
            Value::Bool(_) => "bool",
            Value::Address(_) => "address",
            Value::Bytes32(_) => "bytes32",
            Value::Bytes(_) => "bytes",
            Value::String(_) => "string",
            Value::Tuple(_) => "tuple",
            Value::Array(_) => "array",
            Value::Struct { .. } => "struct",
            Value::Resource { .. } => "resource",
            Value::Ref(_) => "ref",
            Value::RefMut(_) => "ref_mut",
            Value::Unit => "()",
            Value::None => "none",
        }
    }

    /// Try to convert to U256
    pub fn as_u256(&self) -> Option<U256> {
        match self {
            Value::U256(n) => Some(*n),
            Value::Bool(b) => Some(if *b { U256::one() } else { U256::zero() }),
            _ => None,
        }
    }

    /// Try to convert to bool
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::U256(n) => Some(!n.is_zero()),
            _ => None,
        }
    }

    /// Try to convert to address
    pub fn as_address(&self) -> Option<H160> {
        match self {
            Value::Address(a) => Some(*a),
            Value::U256(n) => {
                let mut bytes = [0u8; 32];
                n.to_big_endian(&mut bytes);
                Some(H160::from_slice(&bytes[12..32]))
            }
            _ => None,
        }
    }

    /// Try to convert to bytes
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            Value::Bytes32(h) => Some(h.as_bytes()),
            _ => None,
        }
    }

    /// Check if this is a resource type
    pub fn is_resource(&self) -> bool {
        matches!(self, Value::Resource { .. })
    }

    /// Encode value to 32-byte slot (for storage)
    pub fn to_slot(&self) -> H256 {
        match self {
            Value::U256(n) => {
                let mut bytes = [0u8; 32];
                n.to_big_endian(&mut bytes);
                H256::from(bytes)
            }
            Value::Bool(b) => {
                let mut bytes = [0u8; 32];
                bytes[31] = if *b { 1 } else { 0 };
                H256::from(bytes)
            }
            Value::Address(a) => {
                let mut bytes = [0u8; 32];
                bytes[12..32].copy_from_slice(a.as_bytes());
                H256::from(bytes)
            }
            Value::Bytes32(h) => *h,
            _ => H256::zero(),
        }
    }

    /// Decode value from 32-byte slot
    pub fn from_slot(slot: H256, ty: &ValueType) -> Self {
        match ty {
            ValueType::U256 => {
                Value::U256(U256::from_big_endian(slot.as_bytes()))
            }
            ValueType::Bool => {
                Value::Bool(slot.as_bytes()[31] != 0)
            }
            ValueType::Address => {
                Value::Address(H160::from_slice(&slot.as_bytes()[12..32]))
            }
            ValueType::Bytes32 => Value::Bytes32(slot),
            _ => Value::U256(U256::from_big_endian(slot.as_bytes())),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::U256(n) => write!(f, "{}", n),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Address(a) => write!(f, "0x{:x}", a),
            Value::Bytes32(h) => write!(f, "0x{:x}", h),
            Value::Bytes(b) => write!(f, "0x{}", hex::encode(b)),
            Value::String(s) => write!(f, "\"{}\"", s),
            Value::Tuple(vals) => {
                write!(f, "(")?;
                for (i, v) in vals.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, ")")
            }
            Value::Array(vals) => {
                write!(f, "[")?;
                for (i, v) in vals.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, "]")
            }
            Value::Struct { type_name, fields } => {
                write!(f, "{} {{ ", type_name)?;
                for (i, (name, val)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", name, val)?;
                }
                write!(f, " }}")
            }
            Value::Resource { type_name, id, .. } => {
                write!(f, "Resource<{}>#{}", type_name, id)
            }
            Value::Ref(r) => write!(f, "&{:?}", r),
            Value::RefMut(r) => write!(f, "&mut {:?}", r),
            Value::Unit => write!(f, "()"),
            Value::None => write!(f, "none"),
        }
    }
}

/// Value type descriptor
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    U256,
    U128,
    U64,
    U32,
    U16,
    U8,
    I256,
    I128,
    I64,
    I32,
    I16,
    I8,
    Bool,
    Address,
    Bytes32,
    Bytes,
    String,
    Tuple(Vec<ValueType>),
    Array(Box<ValueType>, usize),
    Slice(Box<ValueType>),
    Struct(String),
    Resource(String, Vec<ResourceAbility>),
    Ref(Box<ValueType>),
    RefMut(Box<ValueType>),
    Unit,
    Never,
}

/// Resource abilities
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAbility {
    Copy,
    Drop,
    Store,
    Key,
}

impl ValueType {
    /// Get the size in bytes for storage layout
    pub fn storage_size(&self) -> usize {
        match self {
            ValueType::U256 | ValueType::I256 | ValueType::Bytes32 => 32,
            ValueType::U128 | ValueType::I128 => 16,
            ValueType::U64 | ValueType::I64 => 8,
            ValueType::U32 | ValueType::I32 => 4,
            ValueType::U16 | ValueType::I16 => 2,
            ValueType::U8 | ValueType::I8 | ValueType::Bool => 1,
            ValueType::Address => 20,
            ValueType::Tuple(types) => types.iter().map(|t| t.storage_size()).sum(),
            ValueType::Array(elem, len) => elem.storage_size() * len,
            _ => 32, // Default to one slot
        }
    }

    /// Check if type is numeric
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            ValueType::U256
                | ValueType::U128
                | ValueType::U64
                | ValueType::U32
                | ValueType::U16
                | ValueType::U8
                | ValueType::I256
                | ValueType::I128
                | ValueType::I64
                | ValueType::I32
                | ValueType::I16
                | ValueType::I8
        )
    }

    /// Check if type is signed
    pub fn is_signed(&self) -> bool {
        matches!(
            self,
            ValueType::I256
                | ValueType::I128
                | ValueType::I64
                | ValueType::I32
                | ValueType::I16
                | ValueType::I8
        )
    }
}
