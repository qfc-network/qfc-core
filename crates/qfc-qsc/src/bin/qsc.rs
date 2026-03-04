//! QFC QuantumScript Compiler CLI
//!
//! Usage:
//!   qsc compile <file> [-o <output>]
//!   qsc fmt <file> [--check] [--write]
//!   qsc check <file>

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use qfc_qsc::{check_only, compile, format_with_config, CompilerOptions, FormatConfig};

#[derive(Parser)]
#[command(name = "qsc")]
#[command(author = "QFC Network")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "QuantumScript compiler and tools")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compile QuantumScript source to bytecode
    Compile {
        /// Input file (use - for stdin)
        file: PathBuf,

        /// Output file
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Enable optimizations
        #[arg(long)]
        optimize: bool,

        /// Enable debug information
        #[arg(long)]
        debug: bool,

        /// EVM compatibility mode
        #[arg(long)]
        evm_compat: bool,
    },

    /// Format QuantumScript source code
    Fmt {
        /// Input file (use - for stdin)
        file: PathBuf,

        /// Check formatting without modifying files
        #[arg(long)]
        check: bool,

        /// Write formatted output back to file
        #[arg(short, long)]
        write: bool,

        /// Indentation size (spaces)
        #[arg(long, default_value = "4")]
        indent: usize,

        /// Use tabs instead of spaces
        #[arg(long)]
        tabs: bool,

        /// Maximum line width
        #[arg(long, default_value = "100")]
        max_width: usize,
    },

    /// Type-check source code without compiling
    Check {
        /// Input file
        file: PathBuf,
    },

    /// Parse source code and print AST (for debugging)
    Parse {
        /// Input file
        file: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Compile {
            file,
            output,
            optimize,
            debug,
            evm_compat,
        } => cmd_compile(file, output, optimize, debug, evm_compat),
        Commands::Fmt {
            file,
            check,
            write,
            indent,
            tabs,
            max_width,
        } => cmd_fmt(file, check, write, indent, tabs, max_width),
        Commands::Check { file } => cmd_check(file),
        Commands::Parse { file } => cmd_parse(file),
    }
}

fn read_input(file: &PathBuf) -> io::Result<String> {
    if file.as_os_str() == "-" {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        Ok(input)
    } else {
        fs::read_to_string(file)
    }
}

fn cmd_compile(
    file: PathBuf,
    output: Option<PathBuf>,
    optimize: bool,
    debug: bool,
    evm_compat: bool,
) -> ExitCode {
    let source = match read_input(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to read input: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let options = CompilerOptions {
        optimize,
        debug_info: debug,
        verify_specs: false,
        evm_compat,
    };

    match compile(&source, &options) {
        Ok(bytecode) => {
            // Serialize bytecode (simple JSON for now)
            let json = match serde_json::to_string_pretty(&bytecode) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("error: failed to serialize bytecode: {}", e);
                    return ExitCode::FAILURE;
                }
            };

            let output_path = output.unwrap_or_else(|| {
                let mut p = file.clone();
                p.set_extension("qbin");
                p
            });

            if output_path.as_os_str() == "-" {
                println!("{}", json);
            } else {
                if let Err(e) = fs::write(&output_path, &json) {
                    eprintln!("error: failed to write output: {}", e);
                    return ExitCode::FAILURE;
                }
                eprintln!("Compiled to {}", output_path.display());
            }

            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn cmd_fmt(
    file: PathBuf,
    check: bool,
    write: bool,
    indent: usize,
    tabs: bool,
    max_width: usize,
) -> ExitCode {
    let source = match read_input(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to read input: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let config = FormatConfig {
        indent_size: indent,
        use_tabs: tabs,
        max_width,
        ..Default::default()
    };

    match format_with_config(&source, &config) {
        Ok(formatted) => {
            if check {
                // Check mode: compare and report
                if source == formatted {
                    ExitCode::SUCCESS
                } else {
                    eprintln!("{}: would be reformatted", file.display());
                    ExitCode::FAILURE
                }
            } else if write && file.as_os_str() != "-" {
                // Write mode: write back to file
                if source == formatted {
                    // No changes needed
                    ExitCode::SUCCESS
                } else {
                    if let Err(e) = fs::write(&file, &formatted) {
                        eprintln!("error: failed to write file: {}", e);
                        return ExitCode::FAILURE;
                    }
                    eprintln!("Formatted {}", file.display());
                    ExitCode::SUCCESS
                }
            } else {
                // Print to stdout
                print!("{}", formatted);
                io::stdout().flush().ok();
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn cmd_check(file: PathBuf) -> ExitCode {
    let source = match read_input(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to read input: {}", e);
            return ExitCode::FAILURE;
        }
    };

    match check_only(&source) {
        Ok(()) => {
            eprintln!("{}: OK", file.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn cmd_parse(file: PathBuf) -> ExitCode {
    let source = match read_input(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to read input: {}", e);
            return ExitCode::FAILURE;
        }
    };

    match qfc_qsc::parse_only(&source) {
        Ok(ast) => {
            println!("{:#?}", ast);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::FAILURE
        }
    }
}
