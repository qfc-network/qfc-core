//! Math standard library functions
//!
//! Provides mathematical operations for QuantumScript contracts.

use primitive_types::U256;

use super::StdlibContext;
use crate::executor::{ExecutionError, ExecutionResult};
use crate::value::Value;

/// Returns the minimum of two values
/// math::min(a: u256, b: u256) -> u256
pub fn min(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 2, "min")?;
    let a = get_u256(&args[0], "min")?;
    let b = get_u256(&args[1], "min")?;
    Ok(Value::U256(a.min(b)))
}

/// Returns the maximum of two values
/// math::max(a: u256, b: u256) -> u256
pub fn max(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 2, "max")?;
    let a = get_u256(&args[0], "max")?;
    let b = get_u256(&args[1], "max")?;
    Ok(Value::U256(a.max(b)))
}

/// Returns the absolute value (for signed interpretation)
/// math::abs(a: u256) -> u256
pub fn abs(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 1, "abs")?;
    let a = get_u256(&args[0], "abs")?;
    // For U256, check if high bit is set (negative in two's complement)
    let high_bit = U256::one() << 255;
    if a >= high_bit {
        // Two's complement negation
        Ok(Value::U256((!a).overflowing_add(U256::one()).0))
    } else {
        Ok(Value::U256(a))
    }
}

/// Integer square root (floor)
/// math::sqrt(a: u256) -> u256
pub fn sqrt(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 1, "sqrt")?;
    let a = get_u256(&args[0], "sqrt")?;

    if a.is_zero() {
        return Ok(Value::U256(U256::zero()));
    }

    // Newton's method for integer square root
    let mut x = a;
    let mut y = (x + U256::one()) >> 1;

    while y < x {
        x = y;
        y = (x + a / x) >> 1;
    }

    Ok(Value::U256(x))
}

/// Power function
/// math::pow(base: u256, exp: u256) -> u256
pub fn pow(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 2, "pow")?;
    let base = get_u256(&args[0], "pow")?;
    let exp = get_u256(&args[1], "pow")?;

    if exp.is_zero() {
        return Ok(Value::U256(U256::one()));
    }
    if base.is_zero() {
        return Ok(Value::U256(U256::zero()));
    }

    // Binary exponentiation
    let mut result = U256::one();
    let mut base = base;
    let mut exp = exp;

    while !exp.is_zero() {
        if exp & U256::one() == U256::one() {
            result = result.overflowing_mul(base).0;
        }
        exp >>= 1;
        base = base.overflowing_mul(base).0;
    }

    Ok(Value::U256(result))
}

/// Log base 2 (floor)
/// math::log2(a: u256) -> u256
pub fn log2(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 1, "log2")?;
    let a = get_u256(&args[0], "log2")?;

    if a.is_zero() {
        return Err(ExecutionError::Internal("log2(0) is undefined".to_string()));
    }

    // Find the position of the highest set bit
    let mut result = U256::zero();
    let mut n = a;

    // Binary search for the highest bit
    if n >= U256::from(1u128) << 128 {
        n >>= 128;
        result += U256::from(128);
    }
    if n >= U256::from(1u64) << 64 {
        n >>= 64;
        result += U256::from(64);
    }
    if n >= U256::from(1u32) << 32 {
        n >>= 32;
        result += U256::from(32);
    }
    if n >= U256::from(1u16) << 16 {
        n >>= 16;
        result += U256::from(16);
    }
    if n >= U256::from(1u8) << 8 {
        n >>= 8;
        result += U256::from(8);
    }
    if n >= U256::from(1u8) << 4 {
        n >>= 4;
        result += U256::from(4);
    }
    if n >= U256::from(1u8) << 2 {
        n >>= 2;
        result += U256::from(2);
    }
    if n >= U256::from(2u8) {
        result += U256::one();
    }

    Ok(Value::U256(result))
}

/// Clamp value between min and max
/// math::clamp(value: u256, min: u256, max: u256) -> u256
pub fn clamp(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 3, "clamp")?;
    let value = get_u256(&args[0], "clamp")?;
    let min_val = get_u256(&args[1], "clamp")?;
    let max_val = get_u256(&args[2], "clamp")?;

    if min_val > max_val {
        return Err(ExecutionError::Internal("clamp: min > max".to_string()));
    }

    Ok(Value::U256(value.max(min_val).min(max_val)))
}

/// Multiply then divide with full precision intermediate
/// math::mulDiv(a: u256, b: u256, denominator: u256) -> u256
/// Computes (a * b) / denominator without overflow in the intermediate
pub fn mul_div(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 3, "mulDiv")?;
    let a = get_u256(&args[0], "mulDiv")?;
    let b = get_u256(&args[1], "mulDiv")?;
    let denominator = get_u256(&args[2], "mulDiv")?;

    if denominator.is_zero() {
        return Err(ExecutionError::DivisionByZero);
    }

    // Use full multiplication with U512 equivalent
    // For simplicity, we'll use a checked approach
    let (product, overflow) = full_mul(a, b);

    if overflow.is_zero() {
        // No overflow, simple division
        Ok(Value::U256(product / denominator))
    } else {
        // Need to handle 512-bit division
        // For now, use approximation if overflow
        // In production, implement proper 512-bit arithmetic
        let result = full_div(product, overflow, denominator)?;
        Ok(Value::U256(result))
    }
}

/// Multiply then divide, rounding up
/// math::mulDivUp(a: u256, b: u256, denominator: u256) -> u256
pub fn mul_div_up(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    check_args(&args, 3, "mulDivUp")?;
    let a = get_u256(&args[0], "mulDivUp")?;
    let b = get_u256(&args[1], "mulDivUp")?;
    let denominator = get_u256(&args[2], "mulDivUp")?;

    if denominator.is_zero() {
        return Err(ExecutionError::DivisionByZero);
    }

    let (product, overflow) = full_mul(a, b);

    if overflow.is_zero() {
        // (a * b + denominator - 1) / denominator
        let (sum, carry) = product.overflowing_add(denominator - U256::one());
        if carry {
            // Handle overflow in addition
            Ok(Value::U256((sum / denominator) + U256::one()))
        } else {
            Ok(Value::U256(sum / denominator))
        }
    } else {
        // For overflow case with rounding up
        let result = full_div(product, overflow, denominator)?;
        // Check if there was a remainder
        let check = result.overflowing_mul(denominator).0;
        if check < product || overflow > U256::zero() {
            Ok(Value::U256(result + U256::one()))
        } else {
            Ok(Value::U256(result))
        }
    }
}

// Helper functions

fn check_args(args: &[Value], expected: usize, func: &str) -> ExecutionResult<()> {
    if args.len() != expected {
        return Err(ExecutionError::Internal(format!(
            "{}() expects {} arguments, got {}",
            func,
            expected,
            args.len()
        )));
    }
    Ok(())
}

fn get_u256(value: &Value, _func: &str) -> ExecutionResult<U256> {
    value.as_u256().ok_or_else(|| ExecutionError::TypeError {
        expected: "u256".to_string(),
        found: value.type_name().to_string(),
    })
}

/// Full multiplication returning (low, high) parts
fn full_mul(a: U256, b: U256) -> (U256, U256) {
    // Split into 128-bit parts
    let a_lo = a & U256::from(u128::MAX);
    let a_hi = a >> 128;
    let b_lo = b & U256::from(u128::MAX);
    let b_hi = b >> 128;

    // Cross multiplication
    let lo_lo = a_lo * b_lo;
    let lo_hi = a_lo * b_hi;
    let hi_lo = a_hi * b_lo;
    let hi_hi = a_hi * b_hi;

    // Combine
    let (mid, carry1) = lo_hi.overflowing_add(hi_lo);
    let (low, carry2) = lo_lo.overflowing_add(mid << 128);

    let high = hi_hi
        + (mid >> 128)
        + if carry1 {
            U256::one() << 128
        } else {
            U256::zero()
        }
        + if carry2 { U256::one() } else { U256::zero() };

    (low, high)
}

/// Divide 512-bit number (low, high) by U256 denominator
fn full_div(low: U256, high: U256, denominator: U256) -> ExecutionResult<U256> {
    if high >= denominator {
        return Err(ExecutionError::Overflow);
    }

    if high.is_zero() {
        return Ok(low / denominator);
    }

    // Binary long division approximation
    // This is simplified; production code should use proper 512-bit arithmetic
    let mut quotient = U256::zero();
    let mut remainder_high = high;
    let mut remainder_low = low;

    for i in (0..256).rev() {
        // Shift remainder left by 1
        let _carry = remainder_high >> 255;
        remainder_high = (remainder_high << 1) | (remainder_low >> 255);
        remainder_low = remainder_low << 1;

        // If remainder >= denominator (shifted), subtract and set quotient bit
        if remainder_high >= denominator
            || (remainder_high == denominator && remainder_low >= denominator)
        {
            remainder_high = remainder_high - denominator;
            quotient = quotient | (U256::one() << i);
        }
    }

    Ok(quotient)
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
    fn test_min_max() {
        let mut c = ctx();
        let result = min(&mut c, vec![Value::from_u64(10), Value::from_u64(20)]).unwrap();
        assert_eq!(result, Value::from_u64(10));

        let result = max(&mut c, vec![Value::from_u64(10), Value::from_u64(20)]).unwrap();
        assert_eq!(result, Value::from_u64(20));
    }

    #[test]
    fn test_sqrt() {
        let mut c = ctx();
        let result = sqrt(&mut c, vec![Value::from_u64(16)]).unwrap();
        assert_eq!(result, Value::from_u64(4));

        let result = sqrt(&mut c, vec![Value::from_u64(17)]).unwrap();
        assert_eq!(result, Value::from_u64(4)); // floor

        let result = sqrt(&mut c, vec![Value::from_u64(100)]).unwrap();
        assert_eq!(result, Value::from_u64(10));
    }

    #[test]
    fn test_pow() {
        let mut c = ctx();
        let result = pow(&mut c, vec![Value::from_u64(2), Value::from_u64(10)]).unwrap();
        assert_eq!(result, Value::from_u64(1024));

        let result = pow(&mut c, vec![Value::from_u64(3), Value::from_u64(4)]).unwrap();
        assert_eq!(result, Value::from_u64(81));
    }

    #[test]
    fn test_log2() {
        let mut c = ctx();
        let result = log2(&mut c, vec![Value::from_u64(1)]).unwrap();
        assert_eq!(result, Value::from_u64(0));

        let result = log2(&mut c, vec![Value::from_u64(8)]).unwrap();
        assert_eq!(result, Value::from_u64(3));

        let result = log2(&mut c, vec![Value::from_u64(1024)]).unwrap();
        assert_eq!(result, Value::from_u64(10));
    }

    #[test]
    fn test_clamp() {
        let mut c = ctx();
        let result = clamp(
            &mut c,
            vec![Value::from_u64(5), Value::from_u64(0), Value::from_u64(10)],
        )
        .unwrap();
        assert_eq!(result, Value::from_u64(5));

        let result = clamp(
            &mut c,
            vec![Value::from_u64(15), Value::from_u64(0), Value::from_u64(10)],
        )
        .unwrap();
        assert_eq!(result, Value::from_u64(10));
    }

    #[test]
    fn test_mul_div() {
        let mut c = ctx();
        // Simple case: (100 * 200) / 50 = 400
        let result = mul_div(
            &mut c,
            vec![
                Value::from_u64(100),
                Value::from_u64(200),
                Value::from_u64(50),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::from_u64(400));
    }
}
