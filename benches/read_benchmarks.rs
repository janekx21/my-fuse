use criterion::{Criterion, criterion_group, criterion_main};
use my_fuse::test_util::TestFixture;
use std::{fs, hint::black_box};

fn bench_read_file(c: &mut Criterion) {
    let fixture = TestFixture::new();
    let file_path = fixture.path().join("test");
    fs::write(&file_path, "test").unwrap();

    c.bench_function("read_file", |b| {
        b.iter(|| {
            let data = fs::read(&file_path).unwrap();
            black_box(data);
        })
    });
}

fn bench_read_file_with_string_conversion(c: &mut Criterion) {
    let fixture = TestFixture::new();
    let file_path = fixture.path().join("test");
    fs::write(&file_path, "test").unwrap();

    c.bench_function("read_file_with_string_conversion", |b| {
        b.iter(|| {
            let data = fs::read(&file_path).unwrap();
            let content = String::from_utf8(data).unwrap();
            black_box(content);
        })
    });
}

fn bench_read_multiple_files(c: &mut Criterion) {
    let fixture = TestFixture::new();
    let files: Vec<_> = (0..10)
        .map(|i| {
            let path = fixture.path().join(format!("test_{}", i));
            fs::write(&path, format!("test content {}", i)).unwrap();
            path
        })
        .collect();

    c.bench_function("read_multiple_files", |b| {
        b.iter(|| {
            for file_path in &files {
                let data = fs::read(file_path).unwrap();
                black_box(data);
            }
        })
    });
}

fn bench_read_different_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_by_file_size");

    // Test different file sizes
    let sizes = vec![
        ("1KB", 1024),
        ("10KB", 10 * 1024),
        ("100KB", 100 * 1024),
        ("1MB", 1024 * 1024),
    ];

    for (size_name, size_bytes) in sizes {
        let fixture = TestFixture::new();
        let file_path = fixture.path().join(format!("test_{}", size_name));
        let content = "x".repeat(size_bytes);
        fs::write(&file_path, &content).unwrap();

        group.bench_function(size_name, |b| {
            b.iter(|| {
                let data = fs::read(&file_path).unwrap();
                black_box(data);
            })
        });
    }

    group.finish();
}

fn bench_concurrent_reads(c: &mut Criterion) {
    let fixture = TestFixture::new();
    let file_path = fixture.path().join("concurrent_test");
    fs::write(&file_path, "test content for concurrent access").unwrap();

    c.bench_function("concurrent_reads", |b| {
        b.iter(|| {
            // Simulate multiple concurrent reads
            let handles: Vec<_> = (0..5)
                .map(|_| {
                    let path = file_path.clone();
                    std::thread::spawn(move || {
                        let data = fs::read(&path).unwrap();
                        black_box(data);
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        })
    });
}

fn bench_read_with_dir_listing(c: &mut Criterion) {
    // Benchmark the full operation from your test
    let fixture = TestFixture::new();
    fs::write(fixture.path().join("test"), "test").unwrap();

    c.bench_function("read_with_dir_listing", |b| {
        b.iter(|| {
            let data = fs::read(fixture.path().join("test")).unwrap();
            let dir_content = fs::read_dir(fixture.path()).unwrap();
            let count = dir_content.count();
            let content = String::from_utf8(data).unwrap();

            black_box((count, content));
        })
    });
}

criterion_group!(
    benches,
    bench_read_file,
    bench_read_file_with_string_conversion,
    bench_read_multiple_files,
    bench_read_different_sizes,
    bench_concurrent_reads,
    bench_read_with_dir_listing
);

criterion_main!(benches);
