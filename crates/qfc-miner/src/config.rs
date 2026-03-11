//! Miner configuration

use std::path::PathBuf;

use clap::Parser;
use qfc_inference::BackendType;

/// QFC AI Inference Miner
#[derive(Parser, Debug, Clone)]
#[command(name = "qfc-miner", about = "QFC Network AI Inference Miner")]
pub struct MinerCli {
    /// Validator/coordinator RPC endpoint
    #[arg(
        long,
        default_value = "http://127.0.0.1:8545",
        env = "QFC_MINER_RPC_URL"
    )]
    pub validator_rpc: String,

    /// Miner wallet address (hex, required unless --generate-wallet)
    #[arg(long, default_value = "", env = "QFC_MINER_WALLET")]
    pub wallet: String,

    /// Inference backend: auto, cuda, metal, cpu
    #[arg(long, default_value = "auto", env = "QFC_MINER_BACKEND")]
    pub backend: String,

    /// Model cache directory
    #[arg(long, default_value = "./models", env = "QFC_MINER_MODEL_DIR")]
    pub model_dir: PathBuf,

    /// Maximum memory usage in MB (0 = auto-detect)
    #[arg(long, default_value = "0", env = "QFC_MINER_MAX_MEMORY")]
    pub max_memory: u64,

    /// Miner private key (hex, must match --wallet address, required unless --generate-wallet)
    #[arg(long, default_value = "", env = "QFC_MINER_PRIVATE_KEY")]
    pub private_key: String,

    /// Comma-separated list of models to keep hot in VRAM
    #[arg(long, default_value = "", env = "QFC_MINER_HOT_MODELS")]
    pub hot_models: String,

    /// Maximum VRAM in MB for warm model layer (0 = auto)
    #[arg(long, default_value = "5000")]
    pub warm_max_mb: u32,

    /// VRAM reserved for system/overhead in MB
    #[arg(long, default_value = "800")]
    pub vram_reserved_mb: u32,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,

    /// Enable TUI dashboard (requires --features tui)
    #[arg(long)]
    pub dashboard: bool,

    /// Generate a new Ed25519 miner wallet and exit
    #[arg(long)]
    pub generate_wallet: bool,

    /// Enable external inference plugins (llama.cpp, whisper.cpp, stable-diffusion.cpp)
    #[arg(long, env = "QFC_MINER_PLUGINS")]
    pub plugins: bool,

    /// Miner tier for inference plugin (1=basic, 2=mid, 3=high)
    #[arg(long, default_value = "1", env = "QFC_MINER_TIER")]
    pub tier: u8,

    /// Comma-separated list of model names for plugin inference
    #[arg(long, default_value = "", env = "QFC_MINER_PLUGIN_MODELS")]
    pub plugin_models: String,

    /// Custom llama.cpp binary path
    #[arg(long, env = "QFC_MINER_LLAMA_PATH")]
    pub llama_path: Option<PathBuf>,

    /// Custom whisper.cpp binary path
    #[arg(long, env = "QFC_MINER_WHISPER_PATH")]
    pub whisper_path: Option<PathBuf>,

    /// Custom stable-diffusion.cpp binary path
    #[arg(long, env = "QFC_MINER_SD_PATH")]
    pub sd_path: Option<PathBuf>,

    /// Number of GPU layers to offload (llama.cpp -ngl)
    #[arg(long, default_value = "999", env = "QFC_MINER_GPU_LAYERS")]
    pub gpu_layers: u32,

    /// Plugin execution timeout in seconds
    #[arg(long, default_value = "300", env = "QFC_MINER_PLUGIN_TIMEOUT")]
    pub plugin_timeout: u64,
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

    /// Build plugin configuration from CLI args
    pub fn plugin_config(&self) -> crate::plugin::PluginConfig {
        let mut binary_paths = std::collections::HashMap::new();
        if let Some(ref p) = self.llama_path {
            binary_paths.insert("llama-cli".to_string(), p.clone());
        }
        if let Some(ref p) = self.whisper_path {
            binary_paths.insert("whisper-cli".to_string(), p.clone());
        }
        if let Some(ref p) = self.sd_path {
            binary_paths.insert("sd".to_string(), p.clone());
        }

        let models: Vec<String> = self
            .plugin_models
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        crate::plugin::PluginConfig {
            enabled: self.plugins,
            tier: self.tier,
            models,
            model_dir: self.model_dir.clone(),
            binary_paths,
            timeout_secs: self.plugin_timeout,
            gpu_layers: self.gpu_layers,
            threads: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(4),
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
