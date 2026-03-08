//! Integration tests for QFC blockchain
//!
//! Each test uses a unique data directory and port to avoid conflicts
//! when running in parallel.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

/// Get the path to the qfc-node binary
fn get_binary_path() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent() // crates/
        .unwrap()
        .parent() // workspace root
        .unwrap()
        .join("target/release/qfc-node")
}

/// Helper to start a node process.
/// Uses port for RPC and port+1000 for metrics to avoid collisions.
fn start_node(port: u16, datadir: &str) -> Child {
    let binary = get_binary_path();
    let metrics_port = port + 1000;
    Command::new(&binary)
        .args([
            "--dev",
            "--datadir",
            datadir,
            "--rpc",
            "--rpc-addr",
            &format!("127.0.0.1:{}", port),
            "--metrics-addr",
            &format!("127.0.0.1:{}", metrics_port),
            "--no-network",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to start node at {:?}: {}", binary, e))
}

/// Helper to make RPC call with retries
fn rpc_call(port: u16, method: &str, params: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args([
            "-s",
            &format!("http://127.0.0.1:{}", port),
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &format!(
                r#"{{"jsonrpc":"2.0","method":"{}","params":{},"id":1}}"#,
                method, params
            ),
        ])
        .output()
        .map_err(|e| e.to_string())?;

    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

/// Extract result from JSON-RPC response (handles both string and object results)
fn extract_result(response: &str) -> Option<String> {
    // Try string result: "result":"0x..."
    if let Some(start) = response.find(r#""result":""#) {
        let rest = &response[start + 10..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    // Try object/number result: "result":{ or "result":0
    if let Some(start) = response.find(r#""result":"#) {
        let rest = &response[start + 9..];
        // Find matching end (could be }, ], or end of object)
        if rest.starts_with('{') || rest.starts_with('[') {
            // For objects/arrays, return the whole thing
            return Some(rest.trim_end_matches('}').to_string() + "}");
        }
    }
    None
}

/// Wait for node RPC to be ready
fn wait_for_rpc(port: u16, timeout_secs: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = rpc_call(port, "eth_chainId", "[]") {
            if resp.contains("result") {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    false
}

/// Wait for at least one block to be produced
fn wait_for_block(port: u16, timeout_secs: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = rpc_call(port, "eth_blockNumber", "[]") {
            if let Some(height) = extract_result(&resp) {
                let h = height.strip_prefix("0x").unwrap_or(&height);
                if let Ok(n) = u64::from_str_radix(h, 16) {
                    if n >= 1 {
                        return true;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    false
}

/// RAII guard that kills the node and cleans up the datadir on drop
struct TestNode {
    child: Child,
    datadir: String,
}

impl TestNode {
    fn new(port: u16, datadir: &str) -> Self {
        let _ = std::fs::remove_dir_all(datadir);
        let child = start_node(port, datadir);
        Self {
            child,
            datadir: datadir.to_string(),
        }
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}

#[test]
fn test_single_node_block_production() {
    let _node = TestNode::new(19545, "/tmp/qfc_integ_blocks");
    assert!(wait_for_rpc(19545, 10), "Node RPC did not become ready");

    // Dev mode has 4 genesis validators but only 1 is active, so it may take
    // several slots (3s each) before it's our turn. Wait up to 45s.
    assert!(
        wait_for_block(19545, 45),
        "No block produced within 45 seconds"
    );
}

#[test]
fn test_chain_id() {
    let _node = TestNode::new(19546, "/tmp/qfc_integ_chainid");
    assert!(wait_for_rpc(19546, 10), "Node RPC did not become ready");

    let response = rpc_call(19546, "eth_chainId", "[]").expect("RPC call failed");
    let chain_id =
        extract_result(&response).unwrap_or_else(|| panic!("No result in: {}", response));

    assert_eq!(chain_id, "0x2328", "Unexpected chain ID: {}", chain_id);
}

#[test]
fn test_get_balance() {
    let _node = TestNode::new(19547, "/tmp/qfc_integ_balance");
    assert!(wait_for_rpc(19547, 10), "Node RPC did not become ready");

    let response = rpc_call(
        19547,
        "eth_getBalance",
        r#"["0x0000000000000000000000000000000000000001", "latest"]"#,
    )
    .expect("RPC call failed");

    assert!(
        response.contains("result"),
        "Expected balance result: {}",
        response
    );
}

#[test]
fn test_gas_price() {
    let _node = TestNode::new(19548, "/tmp/qfc_integ_gasprice");
    assert!(wait_for_rpc(19548, 10), "Node RPC did not become ready");

    let response = rpc_call(19548, "eth_gasPrice", "[]").expect("RPC call failed");
    let gas_price =
        extract_result(&response).unwrap_or_else(|| panic!("No result in: {}", response));

    assert_eq!(
        gas_price, "0x3b9aca00",
        "Unexpected gas price: {}",
        gas_price
    );
}

#[test]
fn test_estimate_gas() {
    let _node = TestNode::new(19549, "/tmp/qfc_integ_estimategas");
    assert!(wait_for_rpc(19549, 10), "Node RPC did not become ready");

    let response = rpc_call(
        19549,
        "eth_estimateGas",
        r#"[{"to": "0x0000000000000000000000000000000000000001", "value": "0x100"}]"#,
    )
    .expect("RPC call failed");

    assert!(
        response.contains("result"),
        "Expected gas estimate: {}",
        response
    );

    let gas = extract_result(&response).unwrap_or_else(|| panic!("No result in: {}", response));
    let gas_str = gas.strip_prefix("0x").unwrap_or(&gas);
    let gas_num = u64::from_str_radix(gas_str, 16).expect("Invalid hex");

    assert!(gas_num >= 21000, "Gas estimate too low: {}", gas_num);
}

#[test]
fn test_get_block_by_number() {
    let _node = TestNode::new(19550, "/tmp/qfc_integ_getblock");
    assert!(wait_for_rpc(19550, 10), "Node RPC did not become ready");

    // Wait for genesis block to be available
    thread::sleep(Duration::from_secs(2));

    let response =
        rpc_call(19550, "eth_getBlockByNumber", r#"["0x0", false]"#).expect("RPC call failed");

    assert!(response.contains("result"), "Expected block: {}", response);
    assert!(
        response.contains("number"),
        "Block should have number field: {}",
        response
    );
    assert!(
        response.contains("hash"),
        "Block should have hash field: {}",
        response
    );
}
