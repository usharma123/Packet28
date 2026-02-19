use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::path::PathBuf;

fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join(rel)
}

fn bench_lcov_ingest(c: &mut Criterion) {
    let content = std::fs::read(fixture_path("lcov/basic.info")).unwrap();
    let ingestor = covy_ingest::get_ingestor(covy_core::CoverageFormat::Lcov);

    c.bench_function("lcov_parse", |b| {
        b.iter(|| {
            let _ = ingestor.parse(black_box(&content));
        })
    });
}

fn bench_cobertura_ingest(c: &mut Criterion) {
    let content = std::fs::read(fixture_path("cobertura/basic.xml")).unwrap();
    let ingestor = covy_ingest::get_ingestor(covy_core::CoverageFormat::Cobertura);

    c.bench_function("cobertura_parse", |b| {
        b.iter(|| {
            let _ = ingestor.parse(black_box(&content));
        })
    });
}

fn bench_jacoco_ingest(c: &mut Criterion) {
    let content = std::fs::read(fixture_path("jacoco/basic.xml")).unwrap();
    let ingestor = covy_ingest::get_ingestor(covy_core::CoverageFormat::JaCoCo);

    c.bench_function("jacoco_parse", |b| {
        b.iter(|| {
            let _ = ingestor.parse(black_box(&content));
        })
    });
}

fn bench_gocov_ingest(c: &mut Criterion) {
    let content = std::fs::read(fixture_path("gocov/basic.out")).unwrap();
    let ingestor = covy_ingest::get_ingestor(covy_core::CoverageFormat::GoCov);

    c.bench_function("gocov_parse", |b| {
        b.iter(|| {
            let _ = ingestor.parse(black_box(&content));
        })
    });
}

criterion_group!(
    benches,
    bench_lcov_ingest,
    bench_cobertura_ingest,
    bench_jacoco_ingest,
    bench_gocov_ingest
);
criterion_main!(benches);
