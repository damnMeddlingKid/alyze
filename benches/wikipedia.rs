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
use stringzilla::stringzilla::Utf8Wordbreaks;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use parquet::{
    file::reader::{FileReader, SerializedFileReader},
    record::{Row, RowAccessor, reader::RowIter},
    schema::types::Type,
};

criterion_group!(benches, wikipedia_benchmark, ascii_benchmark, analysis_benchmark);
criterion_main!(benches);

pub fn wikipedia_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("wikipedia");

    let n_bytes = 64 << 20; // 64 MiB
    let texts = load_n_bytes(n_bytes);

    group.throughput(Throughput::Bytes(n_bytes));
    group.sample_size(16);

    // Sanity: both paths must find the same boundaries on the real corpus, or the benchmarks below
    // are comparing unequal work. The DFA emits one extra breakpoint per text (the leading WB1 at 0).
    {
        let corpus_bytes: usize = texts.iter().map(|t| t.len()).sum();
        let mut dfa_breaks = 0usize;
        for text in &texts {
            uax29::word::tokenize(text, uax29::word::Options::default(), |_, _| {
                dfa_breaks += 1;
                true
            });
        }
        let sz_segments: usize = texts
            .iter()
            .map(|t| Utf8Wordbreaks::<1024>::with_steps(t.as_bytes()).count())
            .sum();
        let sz_covered: usize = texts
            .iter()
            .map(|t| {
                Utf8Wordbreaks::<1024>::with_steps(t.as_bytes())
                    .map(|s| s.len())
                    .sum::<usize>()
            })
            .sum();
        eprintln!(
            "corpus: {corpus_bytes} bytes in {} texts\n\
             dfa breakpoints: {dfa_breaks}\n\
             sz segments:     {sz_segments} (+{} texts = {})\n\
             sz bytes covered: {sz_covered} ({:.2}% of corpus)",
            texts.len(),
            texts.len(),
            sz_segments + texts.len(),
            100.0 * sz_covered as f64 / corpus_bytes as f64,
        );
    }

    // One-off corpus shape report: how much of the input can the ASCII fast path actually cover?
    // The fast path runs inside a maximal run of [a-zA-Z0-9_], and only after the DFA has consumed
    // the run's first byte — so a run of length L contributes L-1 fast-path bytes.
    {
        let total: usize = texts.iter().map(|t| t.len()).sum();
        let (mut fast, mut runs, mut non_ascii, mut hist) = (0usize, 0usize, 0usize, [0usize; 9]);
        for text in &texts {
            let mut run = 0usize;
            for &b in text.as_bytes() {
                if b >= 0x80 {
                    non_ascii += 1;
                }
                let cont = b.is_ascii_alphanumeric() || b == b'_';
                if cont {
                    run += 1;
                } else {
                    if run >= 2 {
                        fast += run - 1;
                    }
                    if run > 0 {
                        runs += 1;
                        hist[run.min(8)] += 1;
                    }
                    run = 0;
                }
            }
            if run >= 2 {
                fast += run - 1;
            }
            if run > 0 {
                runs += 1;
                hist[run.min(8)] += 1;
            }
        }
        eprintln!(
            "corpus {total} bytes | non-ascii {:.2}% | word runs {runs} (mean {:.2} bytes)\n\
             fast-path-eligible bytes: {fast} ({:.1}%) | DFA bytes: {} ({:.1}%)\n\
             run-length histogram 1..=8+: {:?}",
            100.0 * non_ascii as f64 / total as f64,
            (total - (total - fast - runs)) as f64 / runs as f64,
            100.0 * fast as f64 / total as f64,
            total - fast,
            100.0 * (total - fast) as f64 / total as f64,
            hist,
        );
    }

    group.bench_function("word break", |b| {
        b.iter(|| {
            let mut count = 0;
            for text in &texts {
                uax29::word::tokenize(text, uax29::word::Options::default(), |_, _| {
                    count += 1;
                    true
                });
            }
            std::hint::black_box(&count);
        })
    });

    // When `props` is unused, LLVM will optimize it away (which is amazing!), but we also want
    // to benchmark the cost of computing and using this word-like property.
    group.bench_function("word break + word_like", |b| {
        b.iter(|| {
            let mut count = 0;
            let mut word_like = 0;
            for text in &texts {
                uax29::word::tokenize(text, uax29::word::Options::default(), |_, props| {
                    count += 1;
                    if props.is_word_like() {
                        word_like += 1;
                    }
                    true
                });
            }
            std::hint::black_box((&count, &word_like));
        })
    });

    group.bench_function("word break (windowed)", |b| {
        b.iter(|| {
            let mut count = 0;
            for text in &texts {
                uax29::word::tokenize3(text, uax29::word::Options::default(), |_, _| {
                    count += 1;
                    true
                });
            }
            std::hint::black_box(&count);
        })
    });

    group.bench_function("word break (windowed simd)", |b| {
        b.iter(|| {
            let mut count = 0;
            for text in &texts {
                uax29::word::tokenize4(text, uax29::word::Options::default(), |_, _| {
                    count += 1;
                    true
                });
            }
            std::hint::black_box(&count);
        })
    });

    // Same two measurements against the StringZilla-backed segmenter (`tokenize2`), which delegates
    // boundary detection but recomputes properties in a second pass per segment. Compare pairwise:
    // "word break" isolates the segmenter, "+ word_like" adds the cost of that second pass.
    group.bench_function("word break (stringzilla)", |b| {
        b.iter(|| {
            let mut count = 0;
            for text in &texts {
                uax29::word::tokenize2(text, uax29::word::Options::default(), |_, _| {
                    count += 1;
                    true
                });
            }
            std::hint::black_box(&count);
        })
    });

    group.bench_function("word break + word_like (stringzilla)", |b| {
        b.iter(|| {
            let mut count = 0;
            let mut word_like = 0;
            for text in &texts {
                uax29::word::tokenize2(text, uax29::word::Options::default(), |_, props| {
                    count += 1;
                    if props.is_word_like() {
                        word_like += 1;
                    }
                    true
                });
            }
            std::hint::black_box((&count, &word_like));
        })
    });

    // Same work as "word break (stringzilla)" with the callback removed, to see what the
    // per-boundary call costs the optimizer.
    group.bench_function("word break (stringzilla, no callback)", |b| {
        b.iter(|| {
            let mut count = 0usize;
            let mut acc = 0u8;
            for text in &texts {
                let (n, a) = uax29::word::tokenize2_no_callback(text);
                count += n;
                acc |= a;
            }
            std::hint::black_box((&count, &acc));
        })
    });

    // Isolate StringZilla's segmenter from everything alyze adds: no offsets, no properties, no
    // callback — just drain the iterator. Two batch sizes to see how much is FFI-call amortization.
    // Sweep the batch size. Larger amortizes the FFI call, but the iterator holds two
    // `[usize; STEPS]` arrays inline, so past some point the struct stops fitting in registers and
    // every `next()` pays stack traffic for `index`/`suffix`/`count`.
    macro_rules! sweep_steps {
        ($($n:literal),*) => {$(
            group.bench_function(concat!("sz raw segment count (steps=", $n, ")"), |b| {
                b.iter(|| {
                    let mut count = 0usize;
                    for text in &texts {
                        count += Utf8Wordbreaks::<$n>::with_steps(text.as_bytes()).count();
                    }
                    std::hint::black_box(&count);
                })
            });
        )*};
    }
    sweep_steps!(256, 1024, 2048, 4096, 8192, 16384);

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

/// Same word-break comparison restricted to articles containing no non-ASCII bytes at all, to test
/// whether the DFA-vs-StringZilla gap is driven by non-ASCII handling or holds on pure ASCII.
pub fn ascii_benchmark(c: &mut Criterion) {
    let all = load_n_bytes(64 << 20);
    let total: u64 = all.iter().map(|t| t.len() as u64).sum();
    let texts: Vec<String> = all.into_iter().filter(|t| t.is_ascii()).collect();
    let n_bytes: u64 = texts.iter().map(|t| t.len() as u64).sum();
    eprintln!(
        "ascii-only corpus: {} texts, {n_bytes} bytes ({:.1}% of the {total}-byte corpus)",
        texts.len(),
        100.0 * n_bytes as f64 / total as f64,
    );

    let mut group = c.benchmark_group("wikipedia_ascii");
    group.throughput(Throughput::Bytes(n_bytes));
    group.sample_size(16);

    group.bench_function("word break (dfa)", |b| {
        b.iter(|| {
            let mut count = 0;
            for text in &texts {
                uax29::word::tokenize(text, uax29::word::Options::default(), |_, _| {
                    count += 1;
                    true
                });
            }
            std::hint::black_box(&count);
        })
    });

    group.bench_function("word break (stringzilla)", |b| {
        b.iter(|| {
            let mut count = 0;
            for text in &texts {
                uax29::word::tokenize2(text, uax29::word::Options::default(), |_, _| {
                    count += 1;
                    true
                });
            }
            std::hint::black_box(&count);
        })
    });

    group.bench_function("sz raw segment count", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for text in &texts {
                count += Utf8Wordbreaks::<1024>::with_steps(text.as_bytes()).count();
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
