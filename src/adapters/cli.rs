use std::path::{Path, PathBuf};
use std::time::Instant;
use std::{fs, io};

use crate::application::service::{
    benchmark_directory, benchmark_directory_generator_only_strict,
    debug_file_generator_only_strict, pack_file, unpack_file,
};

pub fn run(argv: impl Iterator<Item = String>) -> Result<(), String> {
    let args = argv.collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        // primary verbs
        Some("compress") | Some("pack") => run_compress(&args),
        Some("decompress") | Some("unpack") => run_decompress(&args),
        // diagnostics
        Some("benchmark") => run_benchmark(&args),
        Some("benchmark-generator") => run_benchmark_generator(&args),
        Some("benchmark-generator-debug") => run_benchmark_generator_debug(&args),
        _ => Err(usage()),
    }
}

// ---------------------------------------------------------------------------
// compress
// ---------------------------------------------------------------------------

fn run_compress(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err(usage());
    }
    let input = PathBuf::from(&args[2]);
    // optional output; default is <input>.pack
    let output = match args.get(3) {
        Some(path) if !path.starts_with('-') => PathBuf::from(path),
        _ => {
            let mut p = input.clone();
            let ext = match p.extension().and_then(|e| e.to_str()) {
                Some(e) => format!("{e}.pack"),
                None => "pack".to_string(),
            };
            p.set_extension(ext);
            p
        }
    };
    // optional block_size (4th positional after output, or 3rd if no explicit output)
    let block_size = parse_optional_block_size(
        args.iter().find(|a| a.parse::<usize>().is_ok()),
    )?;

    let original_size = fs::metadata(&input)
        .map_err(|e| format!("cannot stat {}: {e}", input.display()))?
        .len();

    eprint!("Compressing {} ...", input.display());
    let started = Instant::now();
    pack_file(&input, &output, block_size)?;
    let elapsed = started.elapsed();

    let compressed_size = fs::metadata(&output)
        .map_err(|e| format!("cannot stat output {}: {e}", output.display()))?
        .len();

    let ratio = original_size as f64 / compressed_size as f64;
    let saved = 100.0 * (1.0 - compressed_size as f64 / original_size as f64);

    eprintln!(" done ({:.2?})", elapsed);
    println!();
    println!("  Input      : {} ({} bytes)", input.display(), fmt_size(original_size));
    println!("  Output     : {} ({} bytes)", output.display(), fmt_size(compressed_size));
    println!("  Ratio      : {ratio:.2}x");
    println!("  Space saved: {saved:.1}%");
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// decompress
// ---------------------------------------------------------------------------

fn run_decompress(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err(usage());
    }
    let input = PathBuf::from(&args[2]);
    // default output: strip .pack suffix, or append .out
    let output = match args.get(3) {
        Some(path) if !path.starts_with('-') => PathBuf::from(path),
        _ => default_decompress_output(&input),
    };

    eprint!("Decompressing {} ...", input.display());
    let started = Instant::now();
    unpack_file(&input, &output)?;
    let elapsed = started.elapsed();

    let compressed_size = fs::metadata(&input)
        .map_err(|e| format!("cannot stat {}: {e}", input.display()))?
        .len();
    let restored_size = fs::metadata(&output)
        .map_err(|e| format!("cannot stat {}: {e}", output.display()))?
        .len();

    eprintln!(" done ({:.2?})", elapsed);
    println!();
    println!("  Input      : {} ({} bytes)", input.display(), fmt_size(compressed_size));
    println!("  Output     : {} ({} bytes)", output.display(), fmt_size(restored_size));
    println!("  Roundtrip  : OK");
    println!();

    Ok(())
}

fn default_decompress_output(input: &Path) -> PathBuf {
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");
    let stripped = name.strip_suffix(".pack").unwrap_or(name);
    let base = if stripped == name {
        format!("{name}.out")
    } else {
        stripped.to_string()
    };
    input.with_file_name(base)
}

// ---------------------------------------------------------------------------
// benchmark
// ---------------------------------------------------------------------------

fn run_benchmark(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err(usage());
    }
    let path = PathBuf::from(&args[2]);
    let block_size = parse_optional_block_size(args.get(3))?;
    let reports = benchmark_directory(&path, block_size)?;

    println!(
        "{:<24} {:>10} {:>10} {:>8} {:>8} {:<24} {:>10}",
        "file", "original", "packed", "ratio", "layers", "blocks", "roundtrip"
    );
    for report in reports {
        let block_sizes = report
            .layer_summaries
            .iter()
            .map(|item| format!("{}-{}", item.window_min_bits, item.window_max_bits))
            .collect::<Vec<_>>()
            .join(">");
        println!(
            "{:<24} {:>10} {:>10} {:>8.4} {:>8} {:<24} {:>10}",
            report.source_name,
            report.original_size,
            report.packed_size,
            report.ratio,
            report.layer_count,
            block_sizes,
            report.roundtrip_ok
        );
    }
    Ok(())
}

fn run_benchmark_generator(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err(usage());
    }
    let path = PathBuf::from(&args[2]);
    let block_size = parse_optional_block_size(args.get(3))?;
    let reports = benchmark_directory_generator_only_strict(&path, block_size)?;

    println!(
        "{:<24} {:>10} {:>10} {:>8} {:>8} {:<24} {:>10}",
        "file", "original", "packed", "ratio", "layers", "blocks", "roundtrip"
    );
    for report in reports {
        let block_sizes = report
            .layer_summaries
            .iter()
            .map(|item| format!("{}-{}", item.window_min_bits, item.window_max_bits))
            .collect::<Vec<_>>()
            .join(">");
        println!(
            "{:<24} {:>10} {:>10} {:>8.4} {:>8} {:<24} {:>10}",
            report.source_name,
            report.packed_size,
            report.packed_size,
            report.ratio,
            report.layer_count,
            block_sizes,
            report.roundtrip_ok
        );
    }
    Ok(())
}

fn run_benchmark_generator_debug(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err(usage());
    }
    let path = PathBuf::from(&args[2]);
    let block_size = parse_optional_block_size(args.get(3))?;
    let report = debug_file_generator_only_strict(&path, block_size)?;
    println!(
        "{:<8} {:>8} {:<18} {:>6} {:<18} {:>8} {:>8} {:>8} {:>8} {:>8} {:<24}",
        "offset",
        "window",
        "key",
        "steps",
        "terminal",
        "K",
        "seed",
        "branches",
        "ovh",
        "total",
        "status"
    );
    for item in report {
        println!(
            "{:<8} {:>8} {:<18} {:>6} {:<18} {:>8} {:>8} {:>8} {:>8} {:>8} {:<24}",
            item.offset,
            item.window_bytes,
            if item.key_hex.len() > 18 {
                &item.key_hex[..18]
            } else {
                &item.key_hex
            },
            item.steps
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            item.terminal_mode
                .map(|mode| format!("{mode:?}"))
                .unwrap_or_else(|| "-".to_string()),
            item.key_bits,
            item.seed_bits,
            item.branch_bits,
            item.overhead_bits,
            item.total_bits,
            if item.accepted {
                "accepted".to_string()
            } else {
                item.reject_reason
            }
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn parse_optional_block_size(value: Option<&String>) -> Result<Option<usize>, String> {
    match value {
        None => Ok(None),
        Some(raw) => raw
            .parse::<usize>()
            .map(Some)
            .map_err(|error| format!("invalid block size '{raw}': {error}")),
    }
}

fn fmt_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1_073_741_824 => format!("{:.2} GB", b as f64 / 1_073_741_824.0),
        b if b >= 1_048_576 => format!("{:.2} MB", b as f64 / 1_048_576.0),
        b if b >= 1_024 => format!("{:.2} KB", b as f64 / 1_024.0),
        b => format!("{b} B"),
    }
}

fn usage() -> String {
    [
        "Usage:",
        "  pack compress <input> [output]              # compress file",
        "  pack decompress <input> [output]            # decompress file",
        "",
        "  Aliases: 'pack' = compress, 'unpack' = decompress",
        "",
        "  Output defaults:",
        "    compress:   <input>.pack  (e.g. data.bin -> data.bin.pack)",
        "    decompress: strip .pack   (e.g. data.bin.pack -> data.bin)",
        "",
        "  Options:",
        "    [block_size_bytes]  override automatic block size (power of 2, >= 64)",
        "",
        "  Diagnostics:",
        "  pack benchmark <directory> [block_size_bytes]",
        "  pack benchmark-generator <directory> [block_size_bytes]",
        "  pack benchmark-generator-debug <file> [block_size_bytes]",
    ]
    .join("\n")
}
