//! Miner configuration

use std::path::PathBuf;

use clap::Parser;
use qfc_inference::BackendType;

/// QFC AI Inference Miner
#[derive(Parser, Debug, Clone)]
#[command(name = "qfc-miner", about = "QFC Network AI Inference Miner")]
pub struct MinerCli {
    /// Validator/coordinator RPC endpoint
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub validator_rpc: String,

    /// Miner wallet address (hex)
    #[arg(long)]
    pub wallet: String,

    /// Inference backend: auto, cuda, metal, cpu
    #[arg(long, default_value = "auto")]
    pub backend: String,

    /// Model cache directory
    #[arg(long, default_value = "./models")]
    pub model_dir: PathBuf,

    /// Maximum memory usage in MB (0 = auto-detect)
    #[arg(long, default_value = "0")]
    pub max_memory: u64,

    /// Miner private key (hex, must match --wallet address)
    #[arg(long)]
    pub private_key: String,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,
}

impl MinerCli {
    /// Parse backend string into BackendType
    pub fn backend_type(&self) -> BackendType {
        match self.backend.to_lowercase().as_str() {
            "cuda" => BackendType::Cuda,
            "metal" => BackendType::Metal,
            "cpu" => BackendType::Cpu,
            _ => qfc_inference::runtime::detect_backend(), // auto
        }
    }
}

/// Runtime miner configuration (after CLI parsing and hardware detection)
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct MinerConfig {
    /// Validator RPC endpoint
    pub validator_rpc: String,
    /// Miner wallet address
    pub wallet_address: qfc_types::Address,
    /// Selected backend
    pub backend: BackendType,
    /// Model cache directory
    pub model_dir: PathBuf,
    /// Maximum memory in MB
    pub max_memory_mb: u64,
    /// Miner secret key (validated at startup to match wallet_address)
    pub secret_key: [u8; 32],
}
