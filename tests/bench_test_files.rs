/// Integration benchmark: runs compress + decompress on every file in tests/
/// and prints metrics in the format:
///   Файл | N (бит) | original | packed | ratio | roundtrip | window_min-max
///
/// Run with:  cargo test bench_all_test_files -- --nocapture
///
/// NOTE: the "Passes / Peaks / K / V" columns come from the block-level
/// debug path (`benchmark-generator-debug` CLI).  At the service layer we
/// only get the aggregate CompressionReport; that is what we measure here.

use pack::application::service::benchmark_directory;
use std::path::PathBuf;

#[test]
fn bench_all_test_files() {
    let tests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");

    let reports = benchmark_directory(&tests_dir, None)
        .expect("benchmark_directory failed");

    println!();
    println!(
        "{:<32} {:>12} {:>12} {:>10} {:>8} {:>6} {:>26} {:>10}",
        "Файл", "N (бит)", "K+V+OH (бит)", "Ratio", "Layers", "RT?", "Окно (мин-макс бит)", "Expand?"
    );
    println!("{}", "-".repeat(120));

    let mut all_ok = true;

    for report in &reports {
        let n_bits = report.original_size * 8;
        let packed_bits = report.packed_size * 8;
        // ratio as defined in service.rs is packed/original (< 1 means compression)
        let ratio_display = if report.ratio < 1.0 {
            format!("{:.4}x ✓", 1.0 / report.ratio)
        } else {
            format!("{:.4}x ✗", report.ratio)   // expansion
        };
        let window_str = report
            .layer_summaries
            .iter()
            .map(|l| format!("{}-{}", l.window_min_bits, l.window_max_bits))
            .collect::<Vec<_>>()
            .join(">");
        let rt = if report.roundtrip_ok { "OK" } else { "FAIL" };
        let expand = if report.packed_size > report.original_size { "EXPAND" } else { "saved" };

        println!(
            "{:<32} {:>12} {:>12} {:>10} {:>8} {:>6} {:>26} {:>10}",
            report.source_name,
            n_bits,
            packed_bits,
            ratio_display,
            report.layer_count,
            rt,
            window_str,
            expand
        );

        if !report.roundtrip_ok {
            all_ok = false;
            eprintln!("INTEGRITY FAIL: {}", report.source_name);
        }
    }

    println!("{}", "-".repeat(120));
    println!("Total files: {}  |  All roundtrips OK: {}", reports.len(), all_ok);
    println!();

    assert!(
        all_ok,
        "One or more test files failed roundtrip (compress → decompress integrity check)"
    );
}
