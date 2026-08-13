//! A minimal driver for reading the assembly of `tokenize_windowed` with `TokenProperties` live.
//!
//! The benchmark binary contains two monomorphizations of `tokenize_windowed` (one per closure in
//! `word_break_benches!`) plus criterion's machinery, so isolating the one you care about means
//! picking symbols apart by mangled suffix. This example has exactly one call site and one
//! closure, so it emits exactly one `tokenize_windowed` symbol.
//!
//! The callback reads every property bit, which keeps the props computation live — if it only
//! counted tokens, LLVM would delete the property work and you would be reading the wrong code.
//!
//!     cargo build --release --example windowed_props_asm
//!     scripts/dump-asm.sh
//!
//! Pass a file to tokenize, or let it synthesise a corpus:
//!
//!     cargo run --release --example windowed_props_asm -- README.md

use std::hint::black_box;

use alyze::uax29::word::{Options, TokenProperties, tokenize_windowed};

/// `#[inline(never)]` so the loop lands in its own symbol rather than being folded into `main`.
#[inline(never)]
pub fn run(text: &str) -> (u64, u64, u64) {
    let mut tokens = 0u64;
    let mut word_like = 0u64;
    let mut upper = 0u64;

    tokenize_windowed(text, Options::default(), |_bp, props: TokenProperties| {
        tokens += 1;
        if props.is_word_like() {
            word_like += 1;
        }
        if props.has_ascii_upper() {
            upper += 1;
        }
        true
    });

    (tokens, word_like, upper)
}

fn main() {
    let text = match std::env::args().nth(1) {
        Some(path) => std::fs::read_to_string(&path).expect("failed to read input file"),
        // Long enough to exercise the windowed fast path many times over, and mixed enough to
        // hit both the ASCII window and the scalar fallback.
        None => "The quick brown fox jumps over 13 lazy dogs, e.g. U.S.A. isn't far. \
                 Naïve café résumé — 3.14159 and foo_bar_baz.\n"
            .repeat(20_000),
    };

    // `black_box` on the input so the optimiser cannot fold the corpus into constants.
    let (tokens, word_like, upper) = run(black_box(&text));
    println!(
        "bytes={} tokens={tokens} word_like={word_like} has_upper={upper}",
        text.len()
    );
}
