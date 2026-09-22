//! Benchmarks for chunk planning and other performance-critical paths.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use discordfs_core::chunk::plan_chunks;

fn bench_chunk_planning(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunk_planning");

    // Test various file sizes
    let sizes = [
        (1024, 8 * 1024 * 1024, "1KB_file_8MB_chunk"),
        (1024 * 1024, 8 * 1024 * 1024, "1MB_file_8MB_chunk"),
        (8 * 1024 * 1024, 8 * 1024 * 1024, "8MB_file_8MB_chunk"),
        (100 * 1024 * 1024, 8 * 1024 * 1024, "100MB_file_8MB_chunk"),
        (1024 * 1024 * 1024, 8 * 1024 * 1024, "1GB_file_8MB_chunk"),
    ];

    for (file_size, chunk_size, name) in sizes {
        group.bench_with_input(
            BenchmarkId::new("plan_chunks", name),
            &(file_size, chunk_size),
            |b, &(fs, cs)| b.iter(|| plan_chunks(black_box(fs), black_box(cs))),
        );
    }

    group.finish();
}

fn bench_small_chunks(c: &mut Criterion) {
    let mut group = c.benchmark_group("small_chunks");

    // Test with small chunk sizes (stress test)
    let sizes = [
        (1024 * 1024, 1024, "1MB_file_1KB_chunk"),
        (10 * 1024 * 1024, 1024, "10MB_file_1KB_chunk"),
        (1024 * 1024, 4096, "1MB_file_4KB_chunk"),
        (10 * 1024 * 1024, 4096, "10MB_file_4KB_chunk"),
    ];

    for (file_size, chunk_size, name) in sizes {
        group.bench_with_input(
            BenchmarkId::new("plan_chunks", name),
            &(file_size, chunk_size),
            |b, &(fs, cs)| b.iter(|| plan_chunks(black_box(fs), black_box(cs))),
        );
    }

    group.finish();
}

criterion_group!(benches, bench_chunk_planning, bench_small_chunks);
criterion_main!(benches);
