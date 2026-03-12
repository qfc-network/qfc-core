//! Inference backends (CUDA, Metal, ROCm/ONNX, OpenCL, CPU)

pub mod cpu;

#[cfg(feature = "cuda")]
pub mod cuda;

#[cfg(feature = "metal")]
pub mod metal;

#[cfg(feature = "onnx")]
pub mod onnx;

#[cfg(feature = "opencl")]
pub mod opencl;
