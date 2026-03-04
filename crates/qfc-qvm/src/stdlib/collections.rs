//! Collections standard library functions
//!
//! Provides array, bytes, and string operations for QuantumScript contracts.

use primitive_types::U256;

use crate::executor::{ExecutionError, ExecutionResult};
use crate::value::Value;
use super::StdlibContext;

// ============================================================================
// Array operations
// ============================================================================

/// Get array length
/// array::length(arr: array) -> u256
pub fn array_length(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 1, "array::length")?;
    match &args[0] {
        Value::Array(arr) => Ok(Value::from_u64(arr.len() as u64)),
        other => Err(ExecutionError::TypeError {
            expected: "array".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

/// Push element to array (returns new array)
/// array::push(arr: array, element: T) -> array
pub fn array_push(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 2, "array::push")?;
    match &args[0] {
        Value::Array(arr) => {
            let mut new_arr = arr.clone();
            new_arr.push(args[1].clone());
            Ok(Value::Array(new_arr))
        }
        other => Err(ExecutionError::TypeError {
            expected: "array".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

/// Pop element from array (returns tuple of new array and element)
/// array::pop(arr: array) -> (array, T)
pub fn array_pop(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 1, "array::pop")?;
    match &args[0] {
        Value::Array(arr) => {
            if arr.is_empty() {
                return Err(ExecutionError::Internal("array::pop on empty array".to_string()));
            }
            let mut new_arr = arr.clone();
            let element = new_arr.pop().unwrap();
            Ok(Value::Tuple(vec![Value::Array(new_arr), element]))
        }
        other => Err(ExecutionError::TypeError {
            expected: "array".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

/// Get element at index
/// array::get(arr: array, index: u256) -> T
pub fn array_get(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 2, "array::get")?;
    let index = get_u256(&args[1], "array::get")?.as_usize();

    match &args[0] {
        Value::Array(arr) => {
            arr.get(index)
                .cloned()
                .ok_or_else(|| ExecutionError::Internal(format!(
                    "array::get index {} out of bounds (len {})",
                    index, arr.len()
                )))
        }
        other => Err(ExecutionError::TypeError {
            expected: "array".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

/// Set element at index (returns new array)
/// array::set(arr: array, index: u256, value: T) -> array
pub fn array_set(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 3, "array::set")?;
    let index = get_u256(&args[1], "array::set")?.as_usize();

    match &args[0] {
        Value::Array(arr) => {
            if index >= arr.len() {
                return Err(ExecutionError::Internal(format!(
                    "array::set index {} out of bounds (len {})",
                    index, arr.len()
                )));
            }
            let mut new_arr = arr.clone();
            new_arr[index] = args[2].clone();
            Ok(Value::Array(new_arr))
        }
        other => Err(ExecutionError::TypeError {
            expected: "array".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

/// Slice array
/// array::slice(arr: array, start: u256, end: u256) -> array
pub fn array_slice(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 3, "array::slice")?;
    let start = get_u256(&args[1], "array::slice")?.as_usize();
    let end = get_u256(&args[2], "array::slice")?.as_usize();

    match &args[0] {
        Value::Array(arr) => {
            let end = end.min(arr.len());
            let start = start.min(end);
            Ok(Value::Array(arr[start..end].to_vec()))
        }
        other => Err(ExecutionError::TypeError {
            expected: "array".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

/// Concatenate two arrays
/// array::concat(a: array, b: array) -> array
pub fn array_concat(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 2, "array::concat")?;

    match (&args[0], &args[1]) {
        (Value::Array(a), Value::Array(b)) => {
            let mut result = a.clone();
            result.extend(b.clone());
            Ok(Value::Array(result))
        }
        (Value::Array(_), other) => Err(ExecutionError::TypeError {
            expected: "array".to_string(),
            found: other.type_name().to_string(),
        }),
        (other, _) => Err(ExecutionError::TypeError {
            expected: "array".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

// ============================================================================
// Bytes operations
// ============================================================================

/// Get bytes length
/// bytes::length(data: bytes) -> u256
pub fn bytes_length(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 1, "bytes::length")?;
    match &args[0] {
        Value::Bytes(b) => Ok(Value::from_u64(b.len() as u64)),
        Value::Bytes32(_) => Ok(Value::from_u64(32)),
        other => Err(ExecutionError::TypeError {
            expected: "bytes".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

/// Concatenate bytes
/// bytes::concat(a: bytes, b: bytes) -> bytes
pub fn bytes_concat(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 2, "bytes::concat")?;

    let a = get_bytes(&args[0], "bytes::concat")?;
    let b = get_bytes(&args[1], "bytes::concat")?;

    let mut result = a;
    result.extend(b);
    Ok(Value::Bytes(result))
}

/// Slice bytes
/// bytes::slice(data: bytes, start: u256, length: u256) -> bytes
pub fn bytes_slice(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 3, "bytes::slice")?;
    let start = get_u256(&args[1], "bytes::slice")?.as_usize();
    let length = get_u256(&args[2], "bytes::slice")?.as_usize();

    let data = get_bytes(&args[0], "bytes::slice")?;
    let end = (start + length).min(data.len());
    let start = start.min(end);

    Ok(Value::Bytes(data[start..end].to_vec()))
}

// ============================================================================
// String operations
// ============================================================================

/// Get string length (in bytes)
/// string::length(s: string) -> u256
pub fn string_length(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 1, "string::length")?;
    match &args[0] {
        Value::String(s) => Ok(Value::from_u64(s.len() as u64)),
        other => Err(ExecutionError::TypeError {
            expected: "string".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

/// Concatenate strings
/// string::concat(a: string, b: string) -> string
pub fn string_concat(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 2, "string::concat")?;

    let a = get_string(&args[0], "string::concat")?;
    let b = get_string(&args[1], "string::concat")?;

    Ok(Value::String(format!("{}{}", a, b)))
}

/// Slice string (byte-based, may break UTF-8)
/// string::slice(s: string, start: u256, length: u256) -> string
pub fn string_slice(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 3, "string::slice")?;
    let start = get_u256(&args[1], "string::slice")?.as_usize();
    let length = get_u256(&args[2], "string::slice")?.as_usize();

    let s = get_string(&args[0], "string::slice")?;
    let bytes = s.as_bytes();
    let end = (start + length).min(bytes.len());
    let start = start.min(end);

    // Try to create valid UTF-8, replace invalid bytes
    let sliced = &bytes[start..end];
    let result = String::from_utf8_lossy(sliced).into_owned();

    Ok(Value::String(result))
}

// ============================================================================
// Helper functions
// ============================================================================

fn check_args(args: &[Value], expected: usize, func: &str) -> ExecutionResult<()> {
    if args.len() != expected {
        return Err(ExecutionError::Internal(format!(
            "{}() expects {} arguments, got {}",
            func, expected, args.len()
        )));
    }
    Ok(())
}

fn get_u256(value: &Value, _func: &str) -> ExecutionResult<U256> {
    value.as_u256().ok_or_else(|| {
        ExecutionError::TypeError {
            expected: "u256".to_string(),
            found: value.type_name().to_string(),
        }
    })
}

fn get_bytes(value: &Value, _func: &str) -> ExecutionResult<Vec<u8>> {
    match value {
        Value::Bytes(b) => Ok(b.clone()),
        Value::Bytes32(h) => Ok(h.as_bytes().to_vec()),
        other => Err(ExecutionError::TypeError {
            expected: "bytes".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

fn get_string(value: &Value, _func: &str) -> ExecutionResult<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => Err(ExecutionError::TypeError {
            expected: "string".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> StdlibContext<'static> {
        static mut MEM: Vec<u8> = Vec::new();
        StdlibContext {
            address: primitive_types::H160::zero(),
            caller: primitive_types::H160::zero(),
            value: U256::zero(),
            block_number: 0,
            timestamp: 0,
            memory: unsafe { &mut *&raw mut MEM },
        }
    }

    #[test]
    fn test_array_operations() {
        let mut c = ctx();

        // Create array and push
        let arr = Value::Array(vec![Value::from_u64(1), Value::from_u64(2)]);

        let len = array_length(&mut c, vec![arr.clone()]).unwrap();
        assert_eq!(len, Value::from_u64(2));

        let pushed = array_push(&mut c, vec![arr.clone(), Value::from_u64(3)]).unwrap();
        if let Value::Array(a) = pushed {
            assert_eq!(a.len(), 3);
        }

        let got = array_get(&mut c, vec![arr.clone(), Value::from_u64(1)]).unwrap();
        assert_eq!(got, Value::from_u64(2));
    }

    #[test]
    fn test_bytes_operations() {
        let mut c = ctx();

        let bytes = Value::Bytes(vec![1, 2, 3, 4, 5]);

        let len = bytes_length(&mut c, vec![bytes.clone()]).unwrap();
        assert_eq!(len, Value::from_u64(5));

        let concat = bytes_concat(&mut c, vec![
            bytes.clone(),
            Value::Bytes(vec![6, 7]),
        ]).unwrap();
        if let Value::Bytes(b) = concat {
            assert_eq!(b, vec![1, 2, 3, 4, 5, 6, 7]);
        }

        let sliced = bytes_slice(&mut c, vec![
            bytes.clone(),
            Value::from_u64(1),
            Value::from_u64(3),
        ]).unwrap();
        if let Value::Bytes(b) = sliced {
            assert_eq!(b, vec![2, 3, 4]);
        }
    }

    #[test]
    fn test_string_operations() {
        let mut c = ctx();

        let s = Value::String("hello".to_string());

        let len = string_length(&mut c, vec![s.clone()]).unwrap();
        assert_eq!(len, Value::from_u64(5));

        let concat = string_concat(&mut c, vec![
            s.clone(),
            Value::String(" world".to_string()),
        ]).unwrap();
        assert_eq!(concat, Value::String("hello world".to_string()));
    }
}
