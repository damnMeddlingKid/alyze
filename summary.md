# Windowed UAX #29 word tokenizer — working notes

Notes from building and debugging the SIMD windowed fast path for
`src/uax29/word/mod.rs`. Covers the layout conventions, the tests and how to run
them, and the measurement traps that produce confidently wrong numbers.

## What the windowed path is

`tokenize_windowed` runs a SIMD kernel over fixed-size windows of pure-ASCII text
and falls back to the scalar DFA (`tokenize`) for everything else — non-ASCII,
the head and tail of the input, and any window the kernel rejects.

Window processors implement `WindowProcessor`:

| processor | `WINDOW_SIZE` | `MIN_POS` | notes |
|---|---|---|---|
| `Scalar` | 16 | 2 | reference kernel; **disagrees with the DFA in ~2% of windows** |
| `Neon` | 16 | 16 | original NEON kernel, carries `prev_lo`/`prev_hi` between windows |
| `Neon32` | 29 | 2 | current default on aarch64; 32 positions per register |

`MIN_POS` is how much left context `process` reads; the caller must not call it
with a smaller `pos`. `WINDOW_SIZE` is how far `pos` advances on success and how
many bits of `WindowTokens::breaks` are meaningful.

### Why 29

The nibble packing puts one nibble per position, so a 128-bit register holds 32
positions: **2 of left context + 29 window bytes + 1 of lookahead**. The 16-byte
kernel builds its pair of registers with `vuzp1q_u8(current, current)`, which
deinterleaves a vector against itself and leaves 16 of the 32 nibbles as
duplicates. Deinterleaving two *different* lookups fills them.

The win is structural: the rule evaluation is per-*register*, not per-byte, so
the same ~20 NEON ops cover 29 bytes instead of 16. Only the table lookup scales
with byte count (2 loads + 4 `vqtbl4q_u8` instead of 1 + 2).

## Layout conventions

Two conventions meet in this kernel, and mixing them is the single most
productive source of bugs here.

**Register order (what the kernel uses).** Position `p` lives at nibble `p`
counting from the least significant nibble, so byte `i` holds position `2i` in
its **low** nibble and `2i+1` in its **high** nibble. Position order is the
register's own bit order — the identity mapping.

In this layout a neighbour shift is a plain 128-bit shift by 4 bits:

```rust
vextq_u8::<N>(v, zero)      // == v >> (8*N)   lane j receives lane j+N
vextq_u8::<16-N>(zero, v)   // == v << (8*N)   lane j receives lane j-N
```

The `16-N` in the second is the part that is easy to get backwards.

**The alternative (mixed-endian) layout** puts the earlier position in the *high*
nibble. It is self-consistent too, but then a neighbour access is no longer a
uniform shift — it alternates between reaching 1 and 3 positions depending on
parity, and needs a parity blend at every use. Six neighbour accesses per window
pay that; the fold pays it once. Pick the convention that suits the hot consumer.

**Class bits.** `ASCII_CUSTOM_BYTE[b]` packs eight class bits per byte:

```
bit 0 letter   bit 1 mid_num   bit 2 extend   bit 3 mid_let
bit 4 numeric  bit 5 cr        bit 6 lf       bit 7 wseg
```

`full_lo` carries the low nibble of each token, `full_hi` the high nibble.
`vshrq_n_u8(v, k)` lands class bit `k` on bit 0 of *both* nibbles; the bits above
are don't-care and get dropped by the final `& 0x11`.

`ALetter` is exactly `[A-Za-z]` and `Numeric` exactly `[0-9]` — verified against
`ASCII_WORD_BREAK_PROP`, no other ASCII byte carries either class. So
`word_like = letter | numeric` in bit 0 of each nibble, and `_` is correctly
excluded (it is `extend`, not `letter`).

## Gotchas

### 1. Benchmarks silently delete the work being measured

This is the big one. A counting callback lets LLVM replace the per-breakpoint
drain with a popcount:

```rust
// collapses — the loop body doesn't depend on WHICH breakpoint it got,
// so "walk the set bits and increment once each" becomes mask.count_ones()
|_, _| { count += 1; true }

// doesn't collapse — each iteration computes something different, and the
// accumulator is a serial chain with no closed form
|bp, _| { acc = acc.rotate_left(1) ^ bp as u64; true }
```

The difference was **3.4x** (2571 MiB/s vs 748 MiB/s on 64 MiB of wikipedia).
Confirmed by finding `cnt.8b` + `addv.8b` in the collapsed build and not the
other. Both are legitimate numbers — counting tokens is a real workload — but
they must not share a benchmark row name.

`benches/wikipedia.rs`'s `word_break_benches!` macro still uses the counting
shape, so its windowed rows report the collapsed figure.

Measured variants of the consuming case land within 3% of each other, so the
serial chain is not pessimistic here:

| consumer | throughput |
|---|---|
| counting (`count += 1`) | 2715 MiB/s |
| serial chain (`rotate ^ bp`) | 773 MiB/s |
| independent stores (`out[i] = bp`) | 748 MiB/s |
| stores + touch token bytes | 752 MiB/s |

**Partial elision counts too.** A props benchmark that reads only
`is_word_like()` lets the `ascii_upper` work be eliminated — consuming all three
accessors moved the windowed row from 67.1 ms to 70.7 ms. Consume every output
you claim to be measuring, and print the accumulator: if the breaks-only and
+props runs produce the same value, the props were not used.

### 2. Use the DFA as the oracle, never the scalar window kernel

`maybe_process_ascii_window` is a second transcription of the same rules, so a
misreading shared with the NEON kernel cancels out and the test passes while both
are wrong. It is also independently broken: **167,172 of 8,140,782 windows
disagree with the DFA** when swept at every alignment. `tokenize` is what the
UAX #29 conformance suite validates against.

### 3. Register halves that look zero but aren't

`vuzp1q_u8(v, v)` duplicates: lanes 8–15 are a copy of 0–7, not padding. Merging
two such results with `vorrq_u8` silently ORs live data together. This bug
appeared twice — once in `load_byte_info`, once in the `ascii_upper` packing.
`vcombine_u8(vget_low_u8(a), vget_low_u8(b))` concatenates without relying on the
upper halves being anything.

### 4. Token-level tests hide position-level bugs

`token_props` accumulates with `(mask & prop_mask) != 0` over a token's span, so
a property bit at the *wrong offset* still gives the right answer while it stays
inside the same token. The `ascii_upper` packing was wrong in 520 of 620 windows
while every token-level test passed. Test the raw masks per position, or pick
inputs where the displacement crosses a token boundary — capitals spread across
several tokens, deep into the window.

### 5. Short test corpora miss whole bug classes

The padded corpus is short inputs, so with `MIN_POS = 16` most fit only one or
two windows. Running the 64 MiB wikipedia corpus found 1692 duplicate
breakpoints across 547 documents, and a panic
(`deferred_break_pos.take().unwrap()` on `None`) that no test reached. Comparing
*sequences* rather than sets is what surfaced the duplicates — set comparison
showed `extra=0 missing=0`.

### 6. The window and the DFA must agree on the handoff

On exit the DFA resumes at `pos`, so it needs the state that byte `pos-1` leaves
it in. Two bytes of lookback suffice: only `AHLetterMid` and `NumericMid` need
more than one, and both are two-character patterns. Derive it by stepping the
real table rather than maintaining a parallel one:

```rust
let prev  = TABLE[State::Any as usize][ASCII_WORD_BREAK_PROP[bytes[pos-2] as usize] as usize].0;
let state = TABLE[prev as usize][ASCII_WORD_BREAK_PROP[bytes[pos-1] as usize] as usize].0;
```

A hand-written table drifts — an earlier one mapped every digit, `.`, `'` and LF
to `ALetter` because the match arms tested whole `ASCII_CUSTOM_BYTE` values while
most characters set several bits.

If the resume state `is_deferred()`, `deferred_break_pos` and `deferred_props`
must be set too, or `Action::DeferredBreak` panics on `unwrap`. The deferred run
is never longer than one character, so the position is always `pos - 1` — never a
list. Deferred characters are always punctuation, which carries no properties, so
`deferred_props` is always zero in practice.

### 7. Naming shifts meaning

`shl_nibble`/`shr_nibble` describe how bits move *inside a byte*, not which
neighbour you get. Flipping the packing inverted both without a single compile
error, silently breaking six call sites. `prev_pos`/`next_pos` would have made it
a mechanical rename.

## Tests

All in `src/uax29/word/mod.rs`. Run everything with `cargo test --lib`, or one at
a time with `cargo test --lib -- <name>`.

### Kernel-level

| test | what it pins |
|---|---|
| `neon_kernel_matches_dfa_rules` | seven inputs isolating one rule each (WB6/WB7, WB11/WB12, WB13a/b, WB3, WB3d, mixed), swept across **every** window offset so edge-only failures are caught. Runs `Neon` and `Neon32`; failures are prefixed with the processor name. Oracle is `tokenize`. |
| `load_byte_info_packs_register_order` | the 32-byte → `(lo, hi)` packing, against literal expected vectors. Input spans every class the table encodes. Guards `ASCII_CUSTOM_BYTE` first so a table change reports itself rather than looking like a packing bug. Failure output prints got/want as aligned rows with carets under differing lanes. |

```
cargo test --lib -- neon_kernel_matches_dfa_rules
cargo test --lib -- neon_kernel_matches_dfa_rules 2>&1 | grep -E "^  \[|differs at"   # one line per failing rule
cargo test --lib -- load_byte_info_packs_register_order
```

### End-to-end breaks

| test | what it pins |
|---|---|
| `windowed_matches_dfa_on_padded_corpus` | the UAX #29 corpus × 12 pad lengths ≈ 23k inputs, windowed vs DFA |
| `test_windowed_break_against_uax29_tests` | UAX #29 conformance through the windowed path |
| `windowed_matches_dfa_on_sampled_failures` | table of `(input, expected breakpoints, category)`; categories tally pass/fail so a failure names the class. Includes `"window handoff"` cases. |
| `windowed_deferred_break_next_to_non_ascii` | Mid-class punctuation next to non-ASCII, 7 bodies × 40 pads. Asserts breakpoints are **strictly increasing**, which catches duplicates without a DFA comparison. |
| `windowed_does_not_panic_on_deferred_handoff` | two minimal panic repros, including a 1-minimal 383-byte case from wikipedia doc 1123 |

### Token properties

| test | what it pins |
|---|---|
| `windowed_token_props_on_padded_ascii` | 20 bodies × 12 pads = 240 cases, full `TokenProperties` bitfield vs DFA |
| `windowed_token_props_single_case` | one short sentence, per-emit table |
| `windowed_token_props_across_windows` | same sentence extended past one window |
| `windowed_token_props_ascii_upper_across_windows` | `HAS_ASCII_UPPER` specifically |
| `deferred_break_does_not_misattribute_props` | props either side of a deferred break |

The props tests compare the whole bitfield rather than the three accessors, so a
bit neither side exposes still has to match.

## Out-of-tree checks worth repeating

Not tests — they need the 64 MiB wikipedia parquet in `.cache/wikipedia/` and run
from a scratch crate that depends on `alyze` by path.

- **Full-corpus differential.** `tokenize` vs `tokenize_windowed_with::<P, _>`
  per document, comparing sequences, counting extra / missing / duplicate
  breakpoints. Found everything the test suite couldn't.
- **Every-alignment kernel sweep.** Call `process` at `pos += 1` rather than the
  stride and compare the mask against the DFA. This is what proved the NEON
  kernel exact and the scalar one broken.
- **Mask-level property differential.** Compare `word_like` / `ascii_upper` per
  position against `is_ascii_alphanumeric` / `is_ascii_uppercase`, rather than
  through token props.
- **Shrinking.** Delta-debugging (delete chunks of halving size from anywhere, to
  a fixed point) plus explicit "does any prefix / any suffix still fail" checks.
  A single front pass and back pass is not the same thing.

## Current state

- Breaks: `Neon32` matches the DFA exactly on the padded corpus, the UAX #29
  suite, and all 4375 wikipedia documents (23.4M breakpoints, zero duplicates,
  zero panics).
- Properties: `word_like` correct; `ascii_upper` mispositioned — the pairing step
  uses `vextq_u8::<1>`, producing overlapping pairs (byte `i` holds positions `i`
  and `i+1`) where `move_nibble_mask` reads disjoint ones (byte `j` holds `2j`,
  `2j+1`).
- Throughput on 64 MiB, best-of-3, fold-proof callbacks:

| | Neon32 | DFA | speedup |
|---|---|---|---|
| breaks only | 64.7 ms / 989 MiB/s | 121.8 ms / 525 MiB/s | 1.88x |
| breaks + props | 70.7 ms / 905 MiB/s | 126.6 ms / 506 MiB/s | 1.79x |

The props figures are provisional until `ascii_upper` is fixed.

- The `Scalar` window processor is slower than the DFA it wraps (473 vs 498
  MiB/s) and disagrees with it; it is a candidate for removal, at the cost of
  leaving non-aarch64 targets without a fast path until an AVX2 kernel exists.

## Notes for an AVX2 kernel

The tradeoffs invert relative to NEON.

- **Table lookup gets harder.** `vqtbl4q_u8` handles a 64-byte table in one
  instruction; `_mm256_shuffle_epi8` indexes 16 bytes per 128-bit lane and zeroes
  on the high bit. The usual answer is a nibble-split — shuffle on low and high
  nibble separately and combine.
- **Mask extraction gets much easier.** `_mm256_movemask_epi8` gives 32 bits in
  one instruction, so the whole `0x0303…` / `0x000F…` / `0x00FF…` fold cascade
  disappears. Worth reconsidering whether to pack nibbles at all: with a free
  movemask, one class per byte may beat the packing, and then the neighbour-shift
  helpers are not needed either.
- **Lane crossing.** `_mm256_alignr_epi8` operates within each 128-bit half, so a
  neighbour shift across the register needs `_mm256_permute2x128_si256` first.
  Same trap as the `vext` direction, in the same place: prev/next context.
