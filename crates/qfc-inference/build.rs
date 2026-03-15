//! Build script for qfc-inference
//!
//! On Windows with the `cuda` feature enabled, this script locates the CUDA
//! toolkit and emits the linker search paths that `link.exe` needs to find
//! `cudart.lib`, `cublas.lib`, `cusolver.lib`, `curand.lib`, etc.
//!
//! Search order for CUDA:
//!   1. `CUDA_PATH` env var (set by the CUDA installer and most CI actions)
//!   2. `CUDA_HOME` env var (common on Linux, sometimes set on Windows)
//!   3. Well-known default paths

fn main() {
    #[cfg(feature = "cuda")]
    cuda::emit_cuda_link_paths();
}

#[cfg(feature = "cuda")]
mod cuda {
    use std::path::{Path, PathBuf};

    /// Emit `cargo:rustc-link-search=native=<path>` for every CUDA lib
    /// directory we can find. This is critical on Windows where `link.exe`
    /// does not search the CUDA directories by default.
    pub fn emit_cuda_link_paths() {
        let cuda_root = find_cuda_root();

        if let Some(root) = &cuda_root {
            println!("cargo:warning=CUDA root found at: {}", root.display());

            // Emit lib search paths — both lib/x64 (Windows) and lib64 (Linux)
            let lib_candidates = [
                root.join("lib").join("x64"),   // Windows standard
                root.join("lib64"),              // Linux standard
                root.join("lib"),                // Fallback
            ];

            for lib_dir in &lib_candidates {
                if lib_dir.is_dir() {
                    println!(
                        "cargo:rustc-link-search=native={}",
                        lib_dir.display()
                    );
                }
            }

            // Also add the bin directory for DLL discovery on Windows
            #[cfg(target_os = "windows")]
            {
                let bin_dir = root.join("bin");
                if bin_dir.is_dir() {
                    println!(
                        "cargo:rustc-link-search=native={}",
                        bin_dir.display()
                    );
                }
            }
        } else {
            println!("cargo:warning=CUDA root not found. CUDA libraries may fail to link.");
            println!("cargo:warning=Set CUDA_PATH or CUDA_HOME to your CUDA toolkit directory.");
        }

        // Re-run if these env vars change
        println!("cargo:rerun-if-env-changed=CUDA_PATH");
        println!("cargo:rerun-if-env-changed=CUDA_HOME");
    }

    /// Locate the CUDA toolkit root directory
    fn find_cuda_root() -> Option<PathBuf> {
        // 1. CUDA_PATH (Windows installer sets this)
        if let Ok(p) = std::env::var("CUDA_PATH") {
            let path = PathBuf::from(&p);
            if path.is_dir() {
                return Some(path);
            }
        }

        // 2. CUDA_HOME (common on Linux, sometimes set on Windows CI)
        if let Ok(p) = std::env::var("CUDA_HOME") {
            let path = PathBuf::from(&p);
            if path.is_dir() {
                return Some(path);
            }
        }

        // 3. Well-known default paths
        #[cfg(target_os = "windows")]
        {
            // Try versioned paths from newest to oldest
            let program_files = std::env::var("ProgramFiles")
                .unwrap_or_else(|_| r"C:\Program Files".to_string());
            let cuda_base = Path::new(&program_files).join("NVIDIA GPU Computing Toolkit").join("CUDA");

            if cuda_base.is_dir() {
                // Find the highest versioned directory
                if let Ok(entries) = std::fs::read_dir(&cuda_base) {
                    let mut versions: Vec<PathBuf> = entries
                        .filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| p.is_dir())
                        .collect();
                    versions.sort();
                    if let Some(latest) = versions.last() {
                        return Some(latest.clone());
                    }
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            // Linux/macOS: check /usr/local/cuda (symlink to latest)
            let default = PathBuf::from("/usr/local/cuda");
            if default.is_dir() {
                return Some(default);
            }
        }

        None
    }
}
