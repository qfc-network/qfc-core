//! Integration tests for QFC blockchain

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

/// Get the path to the qfc-node binary
fn get_binary_path() -> PathBuf {
    // Navigate from crates/qfc-node to workspace root
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent() // crates/
        .unwrap()
        .parent() // workspace root
        .unwrap()
        .join("target/release/qfc-node")
}

/// Helper to start a node process
fn start_node(args: &[&str]) -> Child {
    let binary = get_binary_path();
    Command::new(&binary)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to start node at {:?}: {}", binary, e))
}

/// Helper to make RPC call
fn rpc_call(port: u16, method: &str, params: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args([
            "-s",
            &format!("http://127.0.0.1:{}", port),
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-d", &format!(r#"{{"jsonrpc":"2.0","method":"{}","params":{},"id":1}}"#, method, params),
        ])
        .output()
        .map_err(|e| e.to_string())?;

    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

/// Extract result from JSON-RPC response
fn extract_result(response: &str) -> Option<String> {
    // Simple extraction - find "result":"..." or "result":...
    if let Some(start) = response.find(r#""result":"#) {
        let rest = &response[start + 10..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Clean up test directories
fn cleanup() {
    let _ = std::fs::remove_dir_all("/tmp/qfc_test1");
    let _ = std::fs::remove_dir_all("/tmp/qfc_test2");
}

#[test]
fn test_single_node_block_production() {
    cleanup();

    // Start a dev node
    let mut node = start_node(&[
        "--dev",
        "--datadir", "/tmp/qfc_test1",
        "--rpc", "--rpc-addr", "127.0.0.1:18545",
        "--no-network",
    ]);

    // Wait for node to start and produce blocks
    thread::sleep(Duration::from_secs(5));

    // Check block number
    let response = rpc_call(18545, "eth_blockNumber", "[]").expect("RPC call failed");
    let height = extract_result(&response).expect("Failed to extract result");

    // Convert hex to number
    let height_str = height.strip_prefix("0x").unwrap_or(&height);
    let block_num = u64::from_str_radix(height_str, 16).expect("Invalid hex");

    assert!(block_num >= 1, "Expected at least 1 block, got {}", block_num);

    // Cleanup
    node.kill().ok();
    cleanup();
}

#[test]
fn test_chain_id() {
    cleanup();

    let mut node = start_node(&[
        "--dev",
        "--datadir", "/tmp/qfc_test1",
        "--rpc", "--rpc-addr", "127.0.0.1:18546",
        "--no-network",
    ]);

    thread::sleep(Duration::from_secs(3));

    let response = rpc_call(18546, "eth_chainId", "[]").expect("RPC call failed");
    let chain_id = extract_result(&response).expect("Failed to extract result");

    // Default chain ID is 9000 = 0x2328
    assert_eq!(chain_id, "0x2328", "Unexpected chain ID: {}", chain_id);

    node.kill().ok();
    cleanup();
}

#[test]
fn test_get_balance() {
    cleanup();

    let mut node = start_node(&[
        "--dev",
        "--datadir", "/tmp/qfc_test1",
        "--rpc", "--rpc-addr", "127.0.0.1:18547",
        "--no-network",
    ]);

    thread::sleep(Duration::from_secs(3));

    // Check balance of dev account (should have funds)
    let response = rpc_call(
        18547,
        "eth_getBalance",
        r#"["0x0000000000000000000000000000000000000001", "latest"]"#
    ).expect("RPC call failed");

    // Should have some balance (dev genesis allocates funds)
    assert!(response.contains("result"), "Expected balance result: {}", response);

    node.kill().ok();
    cleanup();
}

#[test]
fn test_gas_price() {
    cleanup();

    let mut node = start_node(&[
        "--dev",
        "--datadir", "/tmp/qfc_test1",
        "--rpc", "--rpc-addr", "127.0.0.1:18548",
        "--no-network",
    ]);

    thread::sleep(Duration::from_secs(3));

    let response = rpc_call(18548, "eth_gasPrice", "[]").expect("RPC call failed");
    let gas_price = extract_result(&response).expect("Failed to extract result");

    // Should return 1 Gwei = 0x3b9aca00
    assert_eq!(gas_price, "0x3b9aca00", "Unexpected gas price: {}", gas_price);

    node.kill().ok();
    cleanup();
}

#[test]
fn test_estimate_gas() {
    cleanup();

    let mut node = start_node(&[
        "--dev",
        "--datadir", "/tmp/qfc_test1",
        "--rpc", "--rpc-addr", "127.0.0.1:18549",
        "--no-network",
    ]);

    thread::sleep(Duration::from_secs(3));

    // Estimate gas for simple transfer
    let response = rpc_call(
        18549,
        "eth_estimateGas",
        r#"[{"to": "0x0000000000000000000000000000000000000001", "value": "0x100"}]"#
    ).expect("RPC call failed");

    assert!(response.contains("result"), "Expected gas estimate: {}", response);

    let gas = extract_result(&response).expect("Failed to extract result");
    let gas_str = gas.strip_prefix("0x").unwrap_or(&gas);
    let gas_num = u64::from_str_radix(gas_str, 16).expect("Invalid hex");

    // Should be around 21000 + buffer
    assert!(gas_num >= 21000, "Gas estimate too low: {}", gas_num);

    node.kill().ok();
    cleanup();
}

#[test]
fn test_get_block_by_number() {
    cleanup();

    let mut node = start_node(&[
        "--dev",
        "--datadir", "/tmp/qfc_test1",
        "--rpc", "--rpc-addr", "127.0.0.1:18550",
        "--no-network",
    ]);

    thread::sleep(Duration::from_secs(5));

    // Get block 0 (genesis)
    let response = rpc_call(
        18550,
        "eth_getBlockByNumber",
        r#"["0x0", false]"#
    ).expect("RPC call failed");

    assert!(response.contains("result"), "Expected block: {}", response);
    assert!(response.contains("number"), "Block should have number field");
    assert!(response.contains("hash"), "Block should have hash field");

    node.kill().ok();
    cleanup();
}
