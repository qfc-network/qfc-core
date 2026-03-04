//! ABI encoding/decoding standard library functions
//!
//! Provides Ethereum-compatible ABI encoding for QuantumScript contracts.

use primitive_types::{H160, H256, U256};

use super::StdlibContext;
use crate::executor::{ExecutionError, ExecutionResult};
use crate::value::Value;

/// ABI encode values
/// abi::encode(...args) -> bytes
pub fn encode(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    let mut result = Vec::new();

    // First pass: calculate head size (fixed size data + offsets for dynamic data)
    let head_size = args.len() * 32;

    // Second pass: encode
    let mut dynamic_data = Vec::new();

    for arg in args.iter() {
        match arg {
            // Fixed-size types (32 bytes each in head)
            Value::U256(n) => {
                let mut bytes = [0u8; 32];
                n.to_big_endian(&mut bytes);
                result.extend_from_slice(&bytes);
            }
            Value::Bool(b) => {
                let mut bytes = [0u8; 32];
                bytes[31] = if *b { 1 } else { 0 };
                result.extend_from_slice(&bytes);
            }
            Value::Address(a) => {
                let mut bytes = [0u8; 32];
                bytes[12..32].copy_from_slice(a.as_bytes());
                result.extend_from_slice(&bytes);
            }
            Value::Bytes32(h) => {
                result.extend_from_slice(h.as_bytes());
            }

            // Dynamic types (offset in head, data in tail)
            Value::Bytes(data) => {
                // Write offset
                let offset = head_size + dynamic_data.len();
                let mut offset_bytes = [0u8; 32];
                U256::from(offset).to_big_endian(&mut offset_bytes);
                result.extend_from_slice(&offset_bytes);

                // Write length + data to dynamic section
                let mut len_bytes = [0u8; 32];
                U256::from(data.len()).to_big_endian(&mut len_bytes);
                dynamic_data.extend_from_slice(&len_bytes);

                // Pad data to 32-byte boundary
                dynamic_data.extend_from_slice(data);
                let padding = (32 - (data.len() % 32)) % 32;
                dynamic_data.extend(vec![0u8; padding]);
            }
            Value::String(s) => {
                let data = s.as_bytes();

                // Write offset
                let offset = head_size + dynamic_data.len();
                let mut offset_bytes = [0u8; 32];
                U256::from(offset).to_big_endian(&mut offset_bytes);
                result.extend_from_slice(&offset_bytes);

                // Write length + data to dynamic section
                let mut len_bytes = [0u8; 32];
                U256::from(data.len()).to_big_endian(&mut len_bytes);
                dynamic_data.extend_from_slice(&len_bytes);

                dynamic_data.extend_from_slice(data);
                let padding = (32 - (data.len() % 32)) % 32;
                dynamic_data.extend(vec![0u8; padding]);
            }
            Value::Array(arr) => {
                // Write offset
                let offset = head_size + dynamic_data.len();
                let mut offset_bytes = [0u8; 32];
                U256::from(offset).to_big_endian(&mut offset_bytes);
                result.extend_from_slice(&offset_bytes);

                // Write length
                let mut len_bytes = [0u8; 32];
                U256::from(arr.len()).to_big_endian(&mut len_bytes);
                dynamic_data.extend_from_slice(&len_bytes);

                // Encode each element (assuming fixed-size elements)
                for elem in arr {
                    let encoded = encode_single(elem)?;
                    dynamic_data.extend_from_slice(&encoded);
                }
            }
            Value::Tuple(elements) => {
                // Tuples are encoded inline (all elements concatenated)
                for elem in elements {
                    let encoded = encode_single(elem)?;
                    result.extend_from_slice(&encoded);
                }
            }
            _ => {
                // Encode as zero for unsupported types
                result.extend_from_slice(&[0u8; 32]);
            }
        }
    }

    // Append dynamic data
    result.extend(dynamic_data);

    Ok(Value::Bytes(result))
}

/// ABI encode packed (no padding)
/// abi::encodePacked(...args) -> bytes
pub fn encode_packed(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    let mut result = Vec::new();

    for arg in args {
        match arg {
            Value::U256(n) => {
                let mut bytes = [0u8; 32];
                n.to_big_endian(&mut bytes);
                result.extend_from_slice(&bytes);
            }
            Value::Bool(b) => {
                result.push(if b { 1 } else { 0 });
            }
            Value::Address(a) => {
                result.extend_from_slice(a.as_bytes());
            }
            Value::Bytes32(h) => {
                result.extend_from_slice(h.as_bytes());
            }
            Value::Bytes(data) => {
                result.extend_from_slice(&data);
            }
            Value::String(s) => {
                result.extend_from_slice(s.as_bytes());
            }
            _ => {}
        }
    }

    Ok(Value::Bytes(result))
}

/// ABI decode
/// abi::decode(data: bytes, types: string) -> tuple
/// types is comma-separated: "uint256,address,bytes"
pub fn decode(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    if args.len() != 2 {
        return Err(ExecutionError::Internal(
            "abi::decode expects 2 arguments".to_string(),
        ));
    }

    let data = match &args[0] {
        Value::Bytes(b) => b.clone(),
        other => {
            return Err(ExecutionError::TypeError {
                expected: "bytes".to_string(),
                found: other.type_name().to_string(),
            })
        }
    };

    let types_str = match &args[1] {
        Value::String(s) => s.clone(),
        other => {
            return Err(ExecutionError::TypeError {
                expected: "string".to_string(),
                found: other.type_name().to_string(),
            })
        }
    };

    let types: Vec<&str> = types_str.split(',').map(|s| s.trim()).collect();
    let mut results = Vec::new();
    let mut offset = 0;

    for type_name in types {
        if offset + 32 > data.len() {
            break;
        }

        let value = match type_name {
            "uint256" | "uint" | "int256" | "int" => {
                let bytes = &data[offset..offset + 32];
                Value::U256(U256::from_big_endian(bytes))
            }
            "bool" => {
                let byte = data[offset + 31];
                Value::Bool(byte != 0)
            }
            "address" => {
                let bytes = &data[offset + 12..offset + 32];
                Value::Address(H160::from_slice(bytes))
            }
            "bytes32" => {
                let bytes = &data[offset..offset + 32];
                Value::Bytes32(H256::from_slice(bytes))
            }
            "bytes" => {
                // Read offset to dynamic data
                let data_offset = U256::from_big_endian(&data[offset..offset + 32]).as_usize();
                if data_offset + 32 > data.len() {
                    Value::Bytes(Vec::new())
                } else {
                    let len =
                        U256::from_big_endian(&data[data_offset..data_offset + 32]).as_usize();
                    let end = (data_offset + 32 + len).min(data.len());
                    Value::Bytes(data[data_offset + 32..end].to_vec())
                }
            }
            "string" => {
                // Read offset to dynamic data
                let data_offset = U256::from_big_endian(&data[offset..offset + 32]).as_usize();
                if data_offset + 32 > data.len() {
                    Value::String(String::new())
                } else {
                    let len =
                        U256::from_big_endian(&data[data_offset..data_offset + 32]).as_usize();
                    let end = (data_offset + 32 + len).min(data.len());
                    let bytes = &data[data_offset + 32..end];
                    Value::String(String::from_utf8_lossy(bytes).into_owned())
                }
            }
            _ => {
                // Unknown type, try to decode as uint256
                let bytes = &data[offset..offset + 32];
                Value::U256(U256::from_big_endian(bytes))
            }
        };

        results.push(value);
        offset += 32;
    }

    Ok(Value::Tuple(results))
}

/// Encode a function call (selector + encoded args)
/// abi::encodeCall(signature: string, ...args) -> bytes
pub fn encode_call(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    if args.is_empty() {
        return Err(ExecutionError::Internal(
            "abi::encodeCall expects at least 1 argument (signature)".to_string(),
        ));
    }

    let signature = match &args[0] {
        Value::String(s) => s.clone(),
        other => {
            return Err(ExecutionError::TypeError {
                expected: "string".to_string(),
                found: other.type_name().to_string(),
            })
        }
    };

    // Calculate function selector (first 4 bytes of keccak256(signature))
    use tiny_keccak::{Hasher, Keccak};
    let mut hasher = Keccak::v256();
    hasher.update(signature.as_bytes());
    let mut hash = [0u8; 32];
    hasher.finalize(&mut hash);

    let mut result = vec![hash[0], hash[1], hash[2], hash[3]];

    // Encode remaining args
    if args.len() > 1 {
        let encoded = encode(_ctx, args[1..].to_vec())?;
        if let Value::Bytes(data) = encoded {
            result.extend(data);
        }
    }

    Ok(Value::Bytes(result))
}

// Helper function to encode a single value
fn encode_single(value: &Value) -> ExecutionResult<Vec<u8>> {
    let mut bytes = [0u8; 32];
    match value {
        Value::U256(n) => {
            n.to_big_endian(&mut bytes);
            Ok(bytes.to_vec())
        }
        Value::Bool(b) => {
            bytes[31] = if *b { 1 } else { 0 };
            Ok(bytes.to_vec())
        }
        Value::Address(a) => {
            bytes[12..32].copy_from_slice(a.as_bytes());
            Ok(bytes.to_vec())
        }
        Value::Bytes32(h) => Ok(h.as_bytes().to_vec()),
        _ => Ok(bytes.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> StdlibContext<'static> {
        static mut MEM: Vec<u8> = Vec::new();
        StdlibContext {
            address: H160::zero(),
            caller: H160::zero(),
            value: U256::zero(),
            block_number: 0,
            timestamp: 0,
            memory: unsafe { &mut *&raw mut MEM },
        }
    }

    #[test]
    fn test_encode_uint256() {
        let mut c = ctx();
        let result = encode(&mut c, vec![Value::from_u64(42)]).unwrap();

        if let Value::Bytes(data) = result {
            assert_eq!(data.len(), 32);
            assert_eq!(data[31], 42);
        } else {
            panic!("Expected Bytes");
        }
    }

    #[test]
    fn test_encode_address() {
        let mut c = ctx();
        let addr = H160::from_low_u64_be(0x1234);
        let result = encode(&mut c, vec![Value::Address(addr)]).unwrap();

        if let Value::Bytes(data) = result {
            assert_eq!(data.len(), 32);
            // Address should be right-aligned
            assert_eq!(&data[12..32], addr.as_bytes());
        } else {
            panic!("Expected Bytes");
        }
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut c = ctx();

        // Encode
        let original = vec![Value::from_u64(100), Value::Bool(true)];
        let encoded = encode(&mut c, original.clone()).unwrap();

        // Decode
        if let Value::Bytes(data) = encoded {
            let decoded = decode(
                &mut c,
                vec![
                    Value::Bytes(data),
                    Value::String("uint256,bool".to_string()),
                ],
            )
            .unwrap();

            if let Value::Tuple(values) = decoded {
                assert_eq!(values[0], Value::from_u64(100));
                assert_eq!(values[1], Value::Bool(true));
            }
        }
    }

    #[test]
    fn test_encode_packed() {
        let mut c = ctx();
        let result = encode_packed(
            &mut c,
            vec![
                Value::Bool(true),
                Value::Address(H160::from_low_u64_be(0x1234)),
            ],
        )
        .unwrap();

        if let Value::Bytes(data) = result {
            // 1 byte for bool + 20 bytes for address
            assert_eq!(data.len(), 21);
            assert_eq!(data[0], 1);
        } else {
            panic!("Expected Bytes");
        }
    }

    #[test]
    fn test_encode_call() {
        let mut c = ctx();
        let result = encode_call(
            &mut c,
            vec![
                Value::String("transfer(address,uint256)".to_string()),
                Value::Address(H160::from_low_u64_be(0x1234)),
                Value::from_u64(100),
            ],
        )
        .unwrap();

        if let Value::Bytes(data) = result {
            // 4 bytes selector + 32 bytes address + 32 bytes uint256
            assert_eq!(data.len(), 68);
            // transfer(address,uint256) selector = 0xa9059cbb
            assert_eq!(&data[0..4], &[0xa9, 0x05, 0x9c, 0xbb]);
        } else {
            panic!("Expected Bytes");
        }
    }
}
