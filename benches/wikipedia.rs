use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

use alyze::analyze::{
    AnalysisOptions, Analyzer, LanguageWithStopwords, ReusableBuffer, StemmingLanguage,
    StopwordRemoval, TokenizerOptions,
};
use alyze::uax29;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use parquet::{
    file::reader::{FileReader, SerializedFileReader},
    record::{Row, RowAccessor, reader::RowIter},
    schema::types::Type,
};

criterion_group!(benches, wikipedia_benchmark, analysis_benchmark);
criterion_main!(benches);

/// Stamps out the word-break benchmarks for one tokenizer.
///
/// A macro rather than a `&[(&str, fn(..))]` table on purpose: the callback is invoked once per
/// token, so routing it through `&mut dyn FnMut` to make the tokenizers share a signature would
/// add a virtual call to the hottest loop in the measurement. Taking the tokenizer as a path
/// keeps every call statically dispatched and inlinable, exactly as a real caller would get.
macro_rules! word_break_benches {
    ($group:expr, $texts:expr, $name:expr, $tokenize:path $(,)?) => {{
        $group.bench_function(BenchmarkId::new("word break", $name), |b| {
            b.iter(|| {
                let mut acc = 0u64;
                for text in $texts {
                    $tokenize(text, uax29::word::Options::default(), |bp, _| {
                        // Fold `bp` in rather than `count += 1`. A bare increment driven by the
                        // window's break mask is reducible to `count += mask.count_ones()`, and
                        // LLVM does exactly that — it emits `cnt.8b`/`addv.8b` and deletes the
                        // per-token loop, so the row stops measuring per-token dispatch. Summing
                        // the breakpoint costs the same single add and cannot be folded into a
                        // popcount, because the value depends on which bit was set, not how many.
                        acc = acc.wrapping_add(bp as u64);
                        true
                    });
                }
                std::hint::black_box(&acc);
            })
        });

        // When `props` is unused, LLVM will optimize it away (which is amazing!), but we also want
        // to benchmark the cost of computing and using this word-like property.
        $group.bench_function(BenchmarkId::new("word break + word_like", $name), |b| {
            b.iter(|| {
                let mut acc = 0u64;
                let mut word_like = 0u64;
                for text in $texts {
                    $tokenize(text, uax29::word::Options::default(), |bp, props| {
                        // `bp` folded into both accumulators for the reason above; the word_like
                        // one matters just as much, since a bare `word_like += 1` under a flag
                        // that the optimiser can hoist out of the loop collapses to a single add.
                        acc = acc.wrapping_add(bp as u64);
                        if props.is_word_like() {
                            word_like = word_like.wrapping_add(bp as u64);
                        }
                        true
                    });
                }
                std::hint::black_box((&acc, &word_like));
            })
        });
    }};
}

pub fn wikipedia_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("wikipedia");

    let n_bytes = 64 << 20; // 64 MiB
    let texts = load_n_bytes(n_bytes);

    group.throughput(Throughput::Bytes(n_bytes));
    group.sample_size(16);

    word_break_benches!(group, &texts, "dfa", uax29::word::tokenize);
    word_break_benches!(group, &texts, "windowed", uax29::word::tokenize_windowed);
    // Same treatment as "windowed" above — same macro, same call shape, a plain fn rather than a
    // turbofish — so the kernel is the only difference between the two rows.
    #[cfg(target_arch = "aarch64")]
    word_break_benches!(
        group,
        &texts,
        "windowed neon16 fn",
        uax29::word::tokenize_windowed_neon16,
    );

    // One row per window kernel, all reading `props`, so they sit with the "+ word_like" rows
    // rather than the breakpoints-only ones. ("windowed" above is `Neon32`, via the dispatch in
    // `tokenize_windowed`.)
    //
    // These are also where the popcount collapse was found: with `count += 1` and
    // `word_like += 1`, LLVM hoisted the flag test out of the loop and replaced the rest with
    // `count += breaks.count_ones()` (`cnt.8b` + `addv.8b`), so the row measured no per-token work
    // at all. Folding `bp` into both accumulators is what keeps it honest.
    #[cfg(target_arch = "aarch64")]
    macro_rules! kernel_bench {
        ($name:expr, $processor:ty) => {
            // Breakpoints only, for comparison against the "word break" rows above.
            group.bench_function(BenchmarkId::new("word break", $name), |b| {
                b.iter(|| {
                    let mut acc = 0u64;
                    for text in &texts {
                        uax29::word::tokenize_windowed_with::<$processor, _>(
                            text,
                            uax29::word::Options::default(),
                            |bp, _| {
                                acc = acc.wrapping_add(bp as u64);
                                true
                            },
                        );
                    }
                    std::hint::black_box(&acc);
                })
            });

            group.bench_function(BenchmarkId::new("word break + word_like", $name), |b| {
                b.iter(|| {
                    let mut acc = 0u64;
                    let mut word_like = 0u64;
                    for text in &texts {
                        uax29::word::tokenize_windowed_with::<$processor, _>(
                            text,
                            uax29::word::Options::default(),
                            |bp, props| {
                                acc = acc.wrapping_add(bp as u64);
                                if props.is_word_like() {
                                    word_like = word_like.wrapping_add(bp as u64);
                                }
                                true
                            },
                        );
                    }
                    std::hint::black_box(&acc);
                    std::hint::black_box(&word_like);
                })
            });
        };
    }

    #[cfg(target_arch = "aarch64")]
    kernel_bench!("windowed neon", uax29::word::Neon);
    #[cfg(target_arch = "aarch64")]
    kernel_bench!("windowed neon16", uax29::word::Neon16);
    #[cfg(target_arch = "aarch64")]
    kernel_bench!("windowed neon32", uax29::word::Neon32);

    group.bench_function("sentence break", |b| {
        b.iter(|| {
            let mut count = 0;
            for text in &texts {
                uax29::sentence::tokenize(text, uax29::sentence::Options::default(), |_| {
                    count += 1;
                    true
                });
            }
            std::hint::black_box(&count);
        })
    });

    group.finish();
}

pub fn analysis_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("analysis");

    let n_bytes = 64 << 20; // 64 MiB
    let texts = load_n_bytes(n_bytes);

    group.throughput(Throughput::Bytes(n_bytes));
    group.sample_size(16);

    let base = AnalysisOptions {
        tokenizer: TokenizerOptions::UAX29Word(uax29::word::Options::default()),
        maximum_token_length: None,
        case_sensitive: false,
        stopword_removal: None,
        stemming: None,
        ascii_folding: false,
    };

    // Each config exercises an additional stage of the analysis pipeline, so the
    // deltas between rows approximate the marginal cost of each filter.
    let configs: &[(&str, AnalysisOptions)] = &[
        (
            "tokenize only (case sensitive)",
            AnalysisOptions {
                case_sensitive: true,
                ..base
            },
        ),
        ("+ lowercase", base),
        (
            "+ stopwords",
            AnalysisOptions {
                stopword_removal: Some(StopwordRemoval::ForLanguage(
                    LanguageWithStopwords::English,
                )),
                ..base
            },
        ),
        (
            "+ stemming",
            AnalysisOptions {
                stemming: Some(StemmingLanguage::English),
                ..base
            },
        ),
        (
            "full pipeline",
            AnalysisOptions {
                maximum_token_length: Some(40),
                stopword_removal: Some(StopwordRemoval::ForLanguage(
                    LanguageWithStopwords::English,
                )),
                stemming: Some(StemmingLanguage::English),
                ascii_folding: true,
                ..base
            },
        ),
    ];

    for (name, options) in configs {
        assert!(options.valid(), "invalid options for benchmark '{name}'");
        let analyzer = Analyzer::new(*options);
        let mut buffer = ReusableBuffer::new();
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut count = 0;
                for text in &texts {
                    analyzer.analyze(text, &mut buffer, |token| {
                        count += 1;
                        std::hint::black_box(&token.text);
                        true
                    });
                }
                std::hint::black_box(&count);
            })
        });
    }

    group.finish();
}

fn load_n_bytes(n: u64) -> Vec<String> {
    let cache_dir = cache_dir();
    let files_and_urls = parquet_files_and_urls();
    let mut texts = Vec::new();
    let mut total_bytes = 0;
    for (file_name, url) in files_and_urls {
        let file = download_file_with_cache(&file_name, &url, &cache_dir);
        let reader = SerializedFileReader::new(file).expect("failed to create parquet reader");
        let rows = iter_parquet_rows(Box::new(reader), &["text"]);
        for row in rows {
            let text = row.get_string(0).cloned().unwrap();
            total_bytes += text.len() as u64;
            texts.push(text);
            if total_bytes >= n {
                return texts;
            }
        }
    }
    panic!("not enough data in parquet files to reach {} bytes", n);
}

fn cache_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache/wikipedia");
    std::fs::create_dir_all(&dir).expect("failed to create cache directory");
    dir
}

fn parquet_files_and_urls() -> Vec<(String, String)> {
    let mut files_and_urls = Vec::new();
    for i in 0..41 {
        let file = format!("train-{:05}-of-00041.parquet", i);
        let url = format!(
            "https://huggingface.co/datasets/wikimedia/wikipedia/resolve/main/20231101.en/{}?download=true",
            file
        );
        files_and_urls.push((file, url));
    }
    files_and_urls
}

fn download_file_with_cache(file_name: &str, url: &str, cache_dir: &Path) -> File {
    let cache_file = cache_dir.join(file_name);
    if !cache_file.exists() {
        println!(
            "wikipedia: downloading '{}' (from {}) for benchmark",
            file_name, url
        );
        let response = ureq::get(url).call().expect("failed to download file");
        let mut tmp_file = tempfile::Builder::new()
            .tempfile_in(cache_dir)
            .expect("failed to create temporary file");
        std::io::copy(&mut response.into_body().into_reader(), &mut tmp_file)
            .expect("failed to write response body to temporary file");
        tmp_file
            .as_file_mut()
            .flush()
            .expect("failed to flush temporary file");
        tmp_file
            .persist(&cache_file)
            .expect("rename failed to move temporary file to cache");
    }
    File::open(cache_file).expect("failed to open cached file")
}

fn iter_parquet_rows(
    reader: Box<dyn FileReader>,
    column_names: &[&str],
) -> impl Iterator<Item = Row> {
    let parquet_metadata = reader.metadata();
    let fields = parquet_metadata.file_metadata().schema().get_fields();
    let mut selected_fields = fields.to_vec();
    selected_fields.retain(|f| column_names.contains(&f.name()));
    let schema_proj = Type::group_type_builder("schema")
        .with_fields(selected_fields)
        .build()
        .unwrap();
    RowIter::from_file_into(reader)
        .project(Some(schema_proj))
        .unwrap()
        .map(|result| result.unwrap())
}
