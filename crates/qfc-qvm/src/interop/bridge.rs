//! Cross-VM Call Bridge
//!
//! Handles the translation of calls between QVM and EVM execution environments.

use primitive_types::{H160, H256, U256};

use crate::executor::{ExecutionContext, ExecutionError, ExecutionResult};
use crate::value::Value;
use super::{CallType, ContractType, CrossVmCall, CrossVmResult, EvmBackend, InteropManager};

/// Call bridge for QVM -> EVM calls
pub struct CallBridge<'a, E: EvmBackend> {
    manager: &'a mut InteropManager<E>,
    context: &'a ExecutionContext,
}

impl<'a, E: EvmBackend> CallBridge<'a, E> {
    pub fn new(manager: &'a mut InteropManager<E>, context: &'a ExecutionContext) -> Self {
        Self { manager, context }
    }

    /// Call an EVM contract with raw calldata
    pub fn call_raw(
        &mut self,
        target: H160,
        calldata: Vec<u8>,
        value: U256,
        gas_limit: u64,
    ) -> ExecutionResult<CrossVmResult> {
        // Check if target is an EVM contract
        let contract_type = self.manager.get_contract_type(target);
        if contract_type != ContractType::Evm {
            return Err(ExecutionError::Internal(format!(
                "Target {:?} is not an EVM contract",
                target
            )));
        }

        let request = CrossVmCall {
            target,
            call_type: CallType::Call,
            calldata,
            value,
            gas_limit,
        };

        self.manager.call_evm(request)
    }

    /// Static call an EVM contract (read-only)
    pub fn static_call_raw(
        &mut self,
        target: H160,
        calldata: Vec<u8>,
        gas_limit: u64,
    ) -> ExecutionResult<CrossVmResult> {
        let contract_type = self.manager.get_contract_type(target);
        if contract_type != ContractType::Evm {
            return Err(ExecutionError::Internal(format!(
                "Target {:?} is not an EVM contract",
                target
            )));
        }

        let request = CrossVmCall {
            target,
            call_type: CallType::StaticCall,
            calldata,
            value: U256::zero(),
            gas_limit,
        };

        self.manager.call_evm(request)
    }

    /// Call an EVM contract function by signature
    pub fn call_function(
        &mut self,
        target: H160,
        signature: &str,
        args: &[Value],
        value: U256,
        gas_limit: u64,
    ) -> ExecutionResult<CrossVmResult> {
        let selector = self.manager.get_selector(signature);
        let calldata = self.manager.build_calldata(selector, args);

        self.call_raw(target, calldata, value, gas_limit)
    }

    /// Static call an EVM contract function by signature
    pub fn static_call_function(
        &mut self,
        target: H160,
        signature: &str,
        args: &[Value],
        gas_limit: u64,
    ) -> ExecutionResult<CrossVmResult> {
        let selector = self.manager.get_selector(signature);
        let calldata = self.manager.build_calldata(selector, args);

        self.static_call_raw(target, calldata, gas_limit)
    }

    /// Decode return data from an EVM call
    pub fn decode_return(&self, result: &CrossVmResult, return_types: &[&str]) -> Vec<Value> {
        let mut values = Vec::new();
        let mut offset = 0;

        for return_type in return_types {
            if offset + 32 > result.return_data.len() {
                values.push(Value::Unit);
                continue;
            }

            let value = self.manager.decode_from_evm(
                &result.return_data[offset..],
                return_type,
            );
            values.push(value);
            offset += 32;
        }

        values
    }
}

/// Common EVM contract interfaces
pub struct EvmInterfaces;

impl EvmInterfaces {
    // ERC-20 function signatures
    pub const ERC20_BALANCE_OF: &'static str = "balanceOf(address)";
    pub const ERC20_TRANSFER: &'static str = "transfer(address,uint256)";
    pub const ERC20_TRANSFER_FROM: &'static str = "transferFrom(address,address,uint256)";
    pub const ERC20_APPROVE: &'static str = "approve(address,uint256)";
    pub const ERC20_ALLOWANCE: &'static str = "allowance(address,address)";
    pub const ERC20_TOTAL_SUPPLY: &'static str = "totalSupply()";
    pub const ERC20_NAME: &'static str = "name()";
    pub const ERC20_SYMBOL: &'static str = "symbol()";
    pub const ERC20_DECIMALS: &'static str = "decimals()";

    // ERC-721 function signatures
    pub const ERC721_BALANCE_OF: &'static str = "balanceOf(address)";
    pub const ERC721_OWNER_OF: &'static str = "ownerOf(uint256)";
    pub const ERC721_SAFE_TRANSFER_FROM: &'static str = "safeTransferFrom(address,address,uint256)";
    pub const ERC721_TRANSFER_FROM: &'static str = "transferFrom(address,address,uint256)";
    pub const ERC721_APPROVE: &'static str = "approve(address,uint256)";
    pub const ERC721_GET_APPROVED: &'static str = "getApproved(uint256)";
    pub const ERC721_SET_APPROVAL_FOR_ALL: &'static str = "setApprovalForAll(address,bool)";
    pub const ERC721_IS_APPROVED_FOR_ALL: &'static str = "isApprovedForAll(address,address)";

    // ERC-1155 function signatures
    pub const ERC1155_BALANCE_OF: &'static str = "balanceOf(address,uint256)";
    pub const ERC1155_BALANCE_OF_BATCH: &'static str = "balanceOfBatch(address[],uint256[])";
    pub const ERC1155_SAFE_TRANSFER_FROM: &'static str = "safeTransferFrom(address,address,uint256,uint256,bytes)";
    pub const ERC1155_SAFE_BATCH_TRANSFER_FROM: &'static str = "safeBatchTransferFrom(address,address,uint256[],uint256[],bytes)";
    pub const ERC1155_SET_APPROVAL_FOR_ALL: &'static str = "setApprovalForAll(address,bool)";
    pub const ERC1155_IS_APPROVED_FOR_ALL: &'static str = "isApprovedForAll(address,address)";
}

/// ERC-20 helper for QVM contracts
pub struct Erc20Helper<'a, E: EvmBackend> {
    bridge: CallBridge<'a, E>,
    token: H160,
}

impl<'a, E: EvmBackend> Erc20Helper<'a, E> {
    pub fn new(bridge: CallBridge<'a, E>, token: H160) -> Self {
        Self { bridge, token }
    }

    /// Get token balance of an address
    pub fn balance_of(&mut self, account: H160, gas_limit: u64) -> ExecutionResult<U256> {
        let result = self.bridge.static_call_function(
            self.token,
            EvmInterfaces::ERC20_BALANCE_OF,
            &[Value::Address(account)],
            gas_limit,
        )?;

        if !result.success || result.return_data.len() < 32 {
            return Ok(U256::zero());
        }

        Ok(U256::from_big_endian(&result.return_data[0..32]))
    }

    /// Transfer tokens
    pub fn transfer(
        &mut self,
        to: H160,
        amount: U256,
        gas_limit: u64,
    ) -> ExecutionResult<bool> {
        let result = self.bridge.call_function(
            self.token,
            EvmInterfaces::ERC20_TRANSFER,
            &[Value::Address(to), Value::U256(amount)],
            U256::zero(),
            gas_limit,
        )?;

        if !result.success || result.return_data.len() < 32 {
            return Ok(false);
        }

        Ok(result.return_data[31] != 0)
    }

    /// Approve spender
    pub fn approve(
        &mut self,
        spender: H160,
        amount: U256,
        gas_limit: u64,
    ) -> ExecutionResult<bool> {
        let result = self.bridge.call_function(
            self.token,
            EvmInterfaces::ERC20_APPROVE,
            &[Value::Address(spender), Value::U256(amount)],
            U256::zero(),
            gas_limit,
        )?;

        if !result.success || result.return_data.len() < 32 {
            return Ok(false);
        }

        Ok(result.return_data[31] != 0)
    }

    /// Get allowance
    pub fn allowance(
        &mut self,
        owner: H160,
        spender: H160,
        gas_limit: u64,
    ) -> ExecutionResult<U256> {
        let result = self.bridge.static_call_function(
            self.token,
            EvmInterfaces::ERC20_ALLOWANCE,
            &[Value::Address(owner), Value::Address(spender)],
            gas_limit,
        )?;

        if !result.success || result.return_data.len() < 32 {
            return Ok(U256::zero());
        }

        Ok(U256::from_big_endian(&result.return_data[0..32]))
    }

    /// Transfer from (requires prior approval)
    pub fn transfer_from(
        &mut self,
        from: H160,
        to: H160,
        amount: U256,
        gas_limit: u64,
    ) -> ExecutionResult<bool> {
        let result = self.bridge.call_function(
            self.token,
            EvmInterfaces::ERC20_TRANSFER_FROM,
            &[
                Value::Address(from),
                Value::Address(to),
                Value::U256(amount),
            ],
            U256::zero(),
            gas_limit,
        )?;

        if !result.success || result.return_data.len() < 32 {
            return Ok(false);
        }

        Ok(result.return_data[31] != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interop::MockEvmBackend;

    #[test]
    fn test_call_bridge() {
        let mut backend = MockEvmBackend::new();
        backend.deploy(H160::from_low_u64_be(0x1234), vec![0x60, 0x00]);

        let mut manager = InteropManager::new(backend);
        let context = ExecutionContext::default();

        let mut bridge = CallBridge::new(&mut manager, &context);

        let result = bridge.call_raw(
            H160::from_low_u64_be(0x1234),
            vec![],
            U256::zero(),
            100000,
        ).unwrap();

        assert!(result.success);
    }

    #[test]
    fn test_call_function() {
        let mut backend = MockEvmBackend::new();
        backend.deploy(H160::from_low_u64_be(0x1234), vec![0x60, 0x00]);

        let mut manager = InteropManager::new(backend);
        let context = ExecutionContext::default();

        let mut bridge = CallBridge::new(&mut manager, &context);

        let result = bridge.call_function(
            H160::from_low_u64_be(0x1234),
            "transfer(address,uint256)",
            &[
                Value::Address(H160::from_low_u64_be(0x5678)),
                Value::from_u64(100),
            ],
            U256::zero(),
            100000,
        ).unwrap();

        assert!(result.success);
    }
}
