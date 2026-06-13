use std::path::PathBuf;

use crate::application::service::{
    benchmark_directory, benchmark_directory_generator_only_strict,
    debug_file_generator_only_strict, pack_file, unpack_file,
};

pub fn run(argv: impl Iterator<Item = String>) -> Result<(), String> {
    let args = argv.collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        Some("pack") => run_pack(&args),
        Some("unpack") => run_unpack(&args),
        Some("benchmark") => run_benchmark(&args),
        Some("benchmark-generator") => run_benchmark_generator(&args),
        Some("benchmark-generator-debug") => run_benchmark_generator_debug(&args),
        _ => Err(usage()),
    }
}

fn run_pack(args: &[String]) -> Result<(), String> {
    if args.len() < 4 {
        return Err(usage());
    }
    let input = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);
    let block_size = parse_optional_block_size(args.get(4))?;
    pack_file(&input, &output, block_size)
}

fn run_unpack(args: &[String]) -> Result<(), String> {
    if args.len() < 4 {
        return Err(usage());
    }
    let input = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);
    unpack_file(&input, &output)
}

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

fn run_benchmark_generator_debug(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err(usage());
    }
    let path = PathBuf::from(&args[2]);
    let block_size = parse_optional_block_size(args.get(3))?;
    let report = debug_file_generator_only_strict(&path, block_size)?;
    println!(
        "{:<8} {:>8} {:<18} {:>6} {:<18} {:>8} {:>8} {:>8} {:>8} {:<24}",
        "offset", "window", "key", "steps", "terminal", "K", "V", "ovh", "total", "status"
    );
    for item in report {
        println!(
            "{:<8} {:>8} {:<18} {:>6} {:<18} {:>8} {:>8} {:>8} {:>8} {:<24}",
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
            item.crumb_bits,
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

fn parse_optional_block_size(value: Option<&String>) -> Result<Option<usize>, String> {
    match value {
        None => Ok(None),
        Some(raw) => raw
            .parse::<usize>()
            .map(Some)
            .map_err(|error| format!("invalid block size '{raw}': {error}")),
    }
}

fn usage() -> String {
    [
        "Usage:",
        "  pack pack <input> <output> [block_size_bytes]",
        "  pack unpack <input> <output>",
        "  pack benchmark <directory> [block_size_bytes]",
        "  pack benchmark-generator <directory> [block_size_bytes]",
        "  pack benchmark-generator-debug <file> [block_size_bytes]",
    ]
    .join("\n")
}
