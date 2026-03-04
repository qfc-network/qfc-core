//! JIT compiler using Cranelift.

use std::collections::HashMap;
use std::sync::Arc;

use cranelift_codegen::ir::{AbiParam, Function, Signature, UserFuncName};
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_codegen::settings::{self, Configurable, Flags};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use qfc_qsc::Instruction;

use super::codegen::CodeGenerator;
use super::runtime::JitRuntime;
use super::{JitError, JitResult, JitStats};

/// JIT compiler configuration.
#[derive(Debug, Clone)]
pub struct JitConfig {
    /// Optimization level (0-3).
    pub opt_level: u8,
    /// Enable bounds checking.
    pub bounds_check: bool,
    /// Call count threshold before JIT compilation.
    pub jit_threshold: u32,
    /// Maximum code cache size in bytes.
    pub max_cache_size: usize,
}

impl Default for JitConfig {
    fn default() -> Self {
        Self {
            opt_level: 2,
            bounds_check: true,
            jit_threshold: 10,
            max_cache_size: 64 * 1024 * 1024, // 64 MB
        }
    }
}

/// A compiled function that can be executed.
pub struct CompiledFunction {
    /// Function pointer to the compiled code.
    fn_ptr: *const u8,
    /// Size of the compiled code.
    code_size: usize,
    /// Function name for debugging.
    name: String,
}

impl CompiledFunction {
    /// Execute the compiled function.
    ///
    /// # Safety
    ///
    /// The runtime must be properly initialized and the function pointer must be valid.
    pub unsafe fn execute(&self, runtime: &mut JitRuntime) -> JitResult<u64> {
        // The compiled function signature is:
        // fn(runtime: *mut JitRuntime) -> u64
        let func: extern "C" fn(*mut JitRuntime) -> u64 = std::mem::transmute(self.fn_ptr);

        Ok(func(runtime as *mut JitRuntime))
    }

    /// Get the size of the compiled code.
    pub fn code_size(&self) -> usize {
        self.code_size
    }

    /// Get the function name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

// CompiledFunction contains a raw pointer but we only read from it after
// JITModule has finalized the code, so it's safe to send between threads.
unsafe impl Send for CompiledFunction {}
unsafe impl Sync for CompiledFunction {}

/// JIT compiler instance.
pub struct JitCompiler {
    /// Cranelift JIT module.
    module: JITModule,
    /// Target ISA.
    isa: Arc<dyn TargetIsa>,
    /// Compilation context (reused).
    ctx: Context,
    /// Function builder context (reused).
    func_ctx: FunctionBuilderContext,
    /// Configuration.
    config: JitConfig,
    /// Compilation statistics.
    stats: JitStats,
    /// Compiled function cache.
    cache: HashMap<String, CompiledFunction>,
    /// Total cached code size.
    cached_size: usize,
}

impl JitCompiler {
    /// Create a new JIT compiler.
    pub fn new(config: JitConfig) -> JitResult<Self> {
        // Configure Cranelift
        let mut flag_builder = settings::builder();

        // Set optimization level
        let opt_level = match config.opt_level {
            0 => "none",
            1 => "speed",
            2 => "speed",
            _ => "speed_and_size",
        };
        flag_builder
            .set("opt_level", opt_level)
            .map_err(|e| JitError::CraneliftError(format!("failed to set opt_level: {}", e)))?;

        // Enable SIMD if available
        flag_builder.set("enable_simd", "true").ok();

        let flags = Flags::new(flag_builder);

        // Get the native ISA
        let isa = cranelift_native::builder()
            .map_err(|e| JitError::CraneliftError(format!("no native ISA: {}", e)))?
            .finish(flags)
            .map_err(|e| JitError::CraneliftError(format!("ISA error: {}", e)))?;

        // Create JIT module
        let mut builder =
            JITBuilder::with_isa(isa.clone(), cranelift_module::default_libcall_names());

        // Register runtime functions
        JitRuntime::register_symbols(&mut builder);

        let module = JITModule::new(builder);
        let ctx = module.make_context();
        let func_ctx = FunctionBuilderContext::new();

        Ok(Self {
            module,
            isa,
            ctx,
            func_ctx,
            config,
            stats: JitStats::default(),
            cache: HashMap::new(),
            cached_size: 0,
        })
    }

    /// Compile a function to native code.
    pub fn compile(
        &mut self,
        name: &str,
        instructions: &[Instruction],
        param_count: u8,
        local_count: u8,
    ) -> JitResult<&CompiledFunction> {
        // Check cache first
        if self.cache.contains_key(name) {
            self.stats.cache_hits += 1;
            return Ok(self.cache.get(name).unwrap());
        }

        self.stats.cache_misses += 1;

        let start_time = std::time::Instant::now();

        // Build function signature
        // fn(runtime: *mut JitRuntime) -> u64
        let pointer_type = self.isa.pointer_type();
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(pointer_type)); // runtime pointer
        sig.returns
            .push(AbiParam::new(cranelift_codegen::ir::types::I64)); // return value

        // Declare the function
        let func_id = self
            .module
            .declare_function(name, Linkage::Local, &sig)
            .map_err(|e| JitError::CraneliftError(format!("declare error: {}", e)))?;

        // Clear context for reuse
        self.ctx.clear();
        self.ctx.func.signature = sig;
        self.ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        // Generate code
        {
            let builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.func_ctx);
            let codegen = CodeGenerator::new(
                pointer_type,
                param_count,
                local_count,
                self.config.bounds_check,
            );
            let builder = codegen.generate(builder, instructions)?;
            builder.finalize();
        }

        // Compile to machine code
        self.module
            .define_function(func_id, &mut self.ctx)
            .map_err(|e| JitError::CraneliftError(format!("define error: {}", e)))?;

        // Finalize and get code
        self.module
            .finalize_definitions()
            .map_err(|e| JitError::CraneliftError(format!("finalize error: {}", e)))?;

        let code = self.module.get_finalized_function(func_id);
        let code_size = self.ctx.compiled_code().unwrap().code_buffer().len();

        // Update stats
        let compilation_time = start_time.elapsed().as_micros() as u64;
        self.stats.functions_compiled += 1;
        self.stats.compilation_time_us += compilation_time;
        self.stats.code_size_bytes += code_size as u64;

        // Evict old entries if cache is full
        while self.cached_size + code_size > self.config.max_cache_size && !self.cache.is_empty() {
            // Simple eviction: remove first entry
            if let Some(key) = self.cache.keys().next().cloned() {
                if let Some(old) = self.cache.remove(&key) {
                    self.cached_size -= old.code_size;
                }
            }
        }

        // Cache the compiled function
        let compiled = CompiledFunction {
            fn_ptr: code,
            code_size,
            name: name.to_string(),
        };
        self.cached_size += code_size;
        self.cache.insert(name.to_string(), compiled);

        Ok(self.cache.get(name).unwrap())
    }

    /// Get compilation statistics.
    pub fn stats(&self) -> &JitStats {
        &self.stats
    }

    /// Get the number of cached functions.
    pub fn cached_functions(&self) -> usize {
        self.cache.len()
    }

    /// Clear the compilation cache.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.cached_size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_qsc::Opcode;

    #[test]
    fn test_jit_config_default() {
        let config = JitConfig::default();
        assert_eq!(config.opt_level, 2);
        assert!(config.bounds_check);
        assert_eq!(config.jit_threshold, 10);
    }

    #[test]
    fn test_jit_compiler_creation() {
        let config = JitConfig::default();
        let compiler = JitCompiler::new(config);
        assert!(compiler.is_ok());
    }

    #[test]
    fn test_compile_simple_function() {
        let config = JitConfig::default();
        let mut compiler = JitCompiler::new(config).unwrap();

        // Simple function: push 42, return
        let instructions = vec![
            Instruction::with_operand(Opcode::Push, vec![42, 0, 0, 0, 0, 0, 0, 0]),
            Instruction::new(Opcode::Return),
        ];

        let result = compiler.compile("test_func", &instructions, 0, 0);
        assert!(result.is_ok());

        let compiled = result.unwrap();
        assert_eq!(compiled.name(), "test_func");
        assert!(compiled.code_size() > 0);
    }

    #[test]
    fn test_cache_hit() {
        let config = JitConfig::default();
        let mut compiler = JitCompiler::new(config).unwrap();

        let instructions = vec![
            Instruction::with_operand(Opcode::Push, vec![1, 0, 0, 0, 0, 0, 0, 0]),
            Instruction::new(Opcode::Return),
        ];

        // First compilation - cache miss
        compiler
            .compile("cached_func", &instructions, 0, 0)
            .unwrap();
        assert_eq!(compiler.stats().cache_misses, 1);
        assert_eq!(compiler.stats().cache_hits, 0);

        // Second call - cache hit
        compiler
            .compile("cached_func", &instructions, 0, 0)
            .unwrap();
        assert_eq!(compiler.stats().cache_hits, 1);
    }
}
