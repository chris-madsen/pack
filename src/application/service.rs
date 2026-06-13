use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::codec::{
    compress_bytes, compress_bytes_generator_only_strict, compress_bytes_with_outcome,
    decompress_bytes,
};
use crate::domain::model::CompressionReport;

pub fn pack_file(
    input_path: &Path,
    output_path: &Path,
    block_size: Option<usize>,
) -> Result<(), String> {
    let data = fs::read(input_path)
        .map_err(|error| format!("failed to read {}: {error}", input_path.display()))?;
    let packed = compress_bytes(&data, block_size)?;
    fs::write(output_path, packed)
        .map_err(|error| format!("failed to write {}: {error}", output_path.display()))
}

pub fn unpack_file(input_path: &Path, output_path: &Path) -> Result<(), String> {
    let archive = fs::read(input_path)
        .map_err(|error| format!("failed to read {}: {error}", input_path.display()))?;
    let unpacked = decompress_bytes(&archive)?;
    fs::write(output_path, unpacked)
        .map_err(|error| format!("failed to write {}: {error}", output_path.display()))
}

pub fn benchmark_directory(
    path: &Path,
    block_size: Option<usize>,
) -> Result<Vec<CompressionReport>, String> {
    let mut files = fs::read_dir(path)
        .map_err(|error| format!("failed to list {}: {error}", path.display()))?
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .filter(|entry| entry.is_file())
        .collect::<Vec<PathBuf>>();
    files.sort();

    files
        .into_iter()
        .map(|file_path| benchmark_file(&file_path, block_size))
        .collect()
}

pub fn benchmark_file(path: &Path, block_size: Option<usize>) -> Result<CompressionReport, String> {
    let data =
        fs::read(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let outcome = compress_bytes_with_outcome(&data, block_size)?;
    let packed = outcome.archive;
    let unpacked = decompress_bytes(&packed)?;
    let original_size = data.len() as u64;
    let packed_size = packed.len() as u64;
    let ratio = packed_size as f64 / original_size as f64;

    Ok(CompressionReport {
        source_name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string(),
        original_size,
        packed_size,
        ratio,
        layer_count: outcome.layer_summaries.len(),
        layer_summaries: outcome.layer_summaries,
        roundtrip_ok: data == unpacked,
    })
}

pub fn benchmark_directory_generator_only_strict(
    path: &Path,
    block_size: Option<usize>,
) -> Result<Vec<CompressionReport>, String> {
    let mut files = fs::read_dir(path)
        .map_err(|error| format!("failed to list {}: {error}", path.display()))?
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .filter(|entry| entry.is_file())
        .collect::<Vec<PathBuf>>();
    files.sort();

    files
        .into_iter()
        .map(|file_path| benchmark_file_generator_only_strict(&file_path, block_size))
        .collect()
}

pub fn benchmark_file_generator_only_strict(
    path: &Path,
    block_size: Option<usize>,
) -> Result<CompressionReport, String> {
    let data =
        fs::read(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let outcome = compress_bytes_generator_only_strict(&data, block_size)?;
    let packed = outcome.archive;
    let unpacked = decompress_bytes(&packed)?;
    let original_size = data.len() as u64;
    let packed_size = packed.len() as u64;
    let ratio = packed_size as f64 / original_size as f64;

    Ok(CompressionReport {
        source_name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string(),
        original_size,
        packed_size,
        ratio,
        layer_count: outcome.layer_summaries.len(),
        layer_summaries: outcome.layer_summaries,
        roundtrip_ok: data == unpacked,
    })
}
