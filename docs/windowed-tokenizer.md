# How the windowed word tokenizer works

> **Spoiler warning.** This is the answer key for `tokenize3` and `tokenize4`. If you are working
> through `docs/windowed-tokenizer-exercise.md`, don't read this first — it gives away the two
> insights the exercise is built around.

Measured on 64 MiB of English Wikipedia, Apple Silicon:

| Implementation | Throughput | vs DFA |
| --- | --- | --- |
| `tokenize` — character-at-a-time DFA | 517 MiB/s | 1.00x |
| `tokenize3` — windowed, scalar kernel | 597 MiB/s | 1.15x |
| `tokenize4` — windowed, NEON kernel | **1.85 GiB/s** | **3.66x** |

Both windowed variants emit byte-for-byte identical breakpoints and `TokenProperties` to the DFA.
The DFA remains the definition of correctness; the windowed paths are accelerators.

---

## 1. Why the DFA leaves speed on the table

`tokenize` walks one character at a time, consulting `TABLE[state][property]` for each. It has an
ASCII fast path that skips the table while scanning a run of `[a-zA-Z0-9_]`. Measuring the corpus
explains why that isn't enough:

```
corpus 67,110,457 bytes | non-ascii 0.53% | word runs 10,787,539 (mean 5.00 bytes)
fast-path-eligible bytes: 43,127,713 (64.3%)  |  DFA bytes: 23,982,744 (35.7%)
```

Three facts follow:

- **A boundary every 2.87 bytes.** UAX-29 emits a segment for every separator too, so `"Hi, world"`
  is `["Hi", ",", " ", "world"]`. The workload is boundary-bound, not byte-bound.
- **The fast path covers 64% of bytes, in runs of 5.** The other 36% — spaces, commas, periods —
  each cost a full table transition. The expensive minority dominates.
- **The fast path is entered and exited 10.8 million times.** Four iterations then an unpredictable
  exit branch. It never reaches steady state.

The lever is therefore *not* "make the ASCII scan faster." It's "stop making a separate,
state-carrying decision for every character."

## 2. The insight that makes windowing possible

UAX-29 looks stateful, but the state exists almost entirely to serve rules that **cannot occur in
ASCII**:

- **WB4** (`X (Extend | Format | ZWJ)* → X`) forces the machine to remember what preceded a run of
  invisible characters. Every Extend, Format and ZWJ character is above U+007F.
- **WB7a/b/c** need `Hebrew_Letter`. **WB13c/d** need `Katakana`. **WB15/16** need
  `Regional_Indicator`. **WB3c** needs `Extended_Pictographic`. None occur below U+0080.

Strip those and what remains is decidable from **four adjacent character classes** — two back, one
back, current, one ahead — with **no carried state at all**. That is the whole trick. A window of
sixteen boundaries can be decided in parallel because no boundary's answer depends on any other
boundary's answer.

### The ASCII class map

`ASCII_WORD_BREAK_PROP` assigns only thirteen distinct properties across 128 bytes:

| Property | Bytes |
| --- | --- |
| `Other` | controls, `!#$%&()*+-/<=>?@[\]^`{\|}~`, tab (54) |
| `ALetter` | `a-zA-Z` (52) |
| `Numeric` | `0-9` (10) |
| `Newline` | U+000B, U+000C |
| `MidNum` | `,` `;` |
| `LF` / `CR` / `WSegSpace` | U+000A / U+000D / space |
| `DoubleQuote` / `SingleQuote` | `"` / `'` |
| `MidNumLet` / `MidLetter` / `ExtendNumLet` | `.` / `:` / `_` |

Note tab is `Other`, **not** `WSegSpace` — only U+0020 is.

### The class encoding

`ascii_window.rs` packs these into one byte per character, eight bits:

```rust
CLS_W     = 1 << 0   // ALetter | Numeric | ExtendNumLet
CLS_AL    = 1 << 1   // ALetter
CLS_NU    = 1 << 2   // Numeric
CLS_SP    = 1 << 3   // WSegSpace
CLS_CR    = 1 << 4   // CR
CLS_LF    = 1 << 5   // LF  (U+000A only!)
CLS_MIDAL = 1 << 6   // MidLetter | MidNumLet | SingleQuote  — `:` `.` `'`
CLS_MIDNU = 1 << 7   // MidNum    | MidNumLet | SingleQuote  — `,` `;` `.` `'`
```

Two encoding decisions carry most of the weight:

**`CLS_W` is one bit spanning three properties.** Every ordered pair drawn from
`{ALetter, Numeric, ExtendNumLet}` joins — via WB5, WB8, WB9, WB10, WB13a and WB13b. Because it is a
*single shared bit*, "do both characters belong to the set" is one instruction: `p & c & CLS_W != 0`.
Six rules, one AND. Splitting it into three bits would force a much clumsier test.

**`Newline` gets no bit at all.** Nothing joins to it, and "no bits set" already means "always
break," which is precisely WB3a/WB3b. Absence of encoding *is* the encoding.

**`MidNumLet` and `SingleQuote` set both mid-bits** because `.` and `'` bridge letters *and* digits;
`:` only bridges letters, `,;` only digits.

### The seven rules

```rust
const fn is_break(pp: u8, p: u8, c: u8, n: u8) -> bool {
    let joined = (p & c & CLS_W  != 0)                                   // WB5/8/9/10/13a/13b
        | (p & c & CLS_SP != 0)                                          // WB3d
        | (p & CLS_CR != 0)    & (c & CLS_LF != 0)                       // WB3
        | (p & CLS_AL != 0)    & (c & CLS_MIDAL != 0) & (n & CLS_AL != 0) // WB6
        | (p & CLS_MIDAL != 0) & (c & CLS_AL != 0)    & (pp & CLS_AL != 0)// WB7
        | (p & CLS_NU != 0)    & (c & CLS_MIDNU != 0) & (n & CLS_NU != 0) // WB12
        | (p & CLS_MIDNU != 0) & (c & CLS_NU != 0)    & (pp & CLS_NU != 0)// WB11
    !joined
}
```

`pp` and `n` are `0` where no such character exists, which is correct at the text edges because
sot/eot always break.

Worked examples:

- `"don't"` — `n`(AL) `'`(MIDAL) `t`(AL). WB6 joins `n`/`'` because the next is AL; WB7 joins `'`/`t`
  because two back is AL. One token.
- `"3,000"` — WB12 and WB11, symmetrically. One token.
- `"a:b"` — `:` is `MidLetter`, so WB6/WB7 apply. One token, which is correct UAX-29 behaviour.
- `"1:2"` — `:` is `MidLetter`, not `MidNum`, so WB11/WB12 don't apply. Three tokens.
- `"a,b"` — `,` is `MidNum`, not in `MidAL`. Three tokens.

## 3. The driver: `tokenize_windowed`

`tokenize3` and `tokenize4` are both thin wrappers over one driver, parameterised by the window
kernel, so they cannot drift:

```rust
pub fn tokenize3(text, opts, cb) { tokenize_windowed(text, cb, ascii_window::decide_window) }
pub fn tokenize4(text, opts, cb) { tokenize_windowed(text, cb, ascii_window::decide_window_neon) }
```

The driver is the DFA loop with one extra branch at the top. Four things make the handoff sound.

### Entry conditions

```rust
pos >= 1
  && pos + WINDOW <= bytes.len()
  && !state.is_deferred()
  && deferred_break_pos.is_none()
  && !last_was_zwj
  && bytes[pos - 1] < 0x80
  && (pos < 2 || bytes[pos - 2] < 0x80)
```

The last two matter more than they look. The window rules read the two preceding bytes as context.
If either is a UTF-8 continuation byte, classifying it as a character in its own right yields
nonsense — and worse, if the real preceding character is a non-ASCII `ALetter` like `é`, WB7 should
fire and the window would wrongly break. Requiring ASCII context is what keeps the rule set honest.

`!state.is_deferred()` matters because the machine mid-lookahead holds a pending breakpoint the
window path knows nothing about.

### The deferred back-off

WB6/WB7 and WB11/WB12 need a character the window may not contain. A window must therefore never
*end* on a bridge character, or the scalar path would have to resume in a deferred state without the
pending breakpoint:

```rust
const fn defers(p: u8, c: u8) -> bool {
    ((p & CLS_AL != 0) & (c & CLS_MIDAL != 0)) | ((p & CLS_NU != 0) & (c & CLS_MIDNU != 0))
}
```

If the last two bytes would defer, commit 15 instead of 16. One back-off always suffices: the
character *before* a bridge is by definition a letter or digit, which never defers.

There's a pleasing consequence. The lookahead `n` for lane 15 is only ever consulted when lane 15 is
a bridge character — exactly the case the back-off discards. So `n` can safely be `0` when the byte
after the window is non-ASCII.

### The state handoff

After committing, the machine must resume in the state it would have reached. Almost all of it comes
from the class bits, with one exception:

```rust
fn state_after_ascii(cls: u8, b: u8) -> State {
    if cls & CLS_AL != 0 { State::ALetter }
    else if cls & CLS_NU != 0 { State::Numeric }
    else if cls & CLS_W  != 0 { State::ExtendNumLet }   // `_`, the only member of W left
    else if cls & CLS_SP != 0 { State::WSegSpace }
    else if cls & CLS_CR != 0 { State::CR }
    else if cls & CLS_LF != 0 { State::Newline }
    else if b.wrapping_sub(0x0b) <= 1 { State::Newline } // U+000B/U+000C carry no class bits
    else { State::Any }
}
```

`Newline` needs no class bit for boundary decisions but *does* need a distinct state, because WB3a
stops Extend/Format/ZWJ from attaching after a newline — and those are non-ASCII, so they arrive on
the scalar path afterwards. The byte is only consulted on the fallthrough.

### Properties without a second pass

The naive way to attribute `WORD_LIKE` and `HAS_ASCII_UPPER` to each token is to re-scan its bytes.
Instead the kernel returns two more bitmasks — bit *i* set if byte *i* is `[a-zA-Z0-9]` / `[A-Z]` —
and the driver tests **ranges of bits**:

```rust
let span = lane_mask(seg_start, lane);          // bits seg_start..lane
if d.word_like & span != 0 { token_props |= TokenProperties::WORD_LIKE; }
if d.upper     & span != 0 { token_props.0 |= TokenProperties::HAS_ASCII_UPPER_MASK; }
```

"Does this token contain an uppercase letter" becomes an AND against a mask. No re-reading of text.

### The drain

```rust
let mut remaining = d.breaks & lane_mask(0, commit);
let mut seg_start = 0usize;
while remaining != 0 {
    let lane = remaining.trailing_zeros() as usize;
    remaining &= remaining - 1;                 // clear lowest set bit
    /* ...props range test, emit callback... */
    seg_start = lane;
}
```

`remaining &= remaining - 1` clears the lowest set bit, so the loop runs once per boundary rather
than sixteen times per window. Note that `trailing_zeros` is the right direction here only because
bit *i* corresponds to lane *i* — LSB-first, matching forward scan order.

## 4. The NEON kernel

`tokenize3`'s kernel builds the same three masks with a scalar loop. `tokenize4` replaces it. Both
satisfy the identical `Decisions` contract, and `neon_matches_scalar` cross-checks them over ~2.1M
combinations of window content and carried context.

**Load and reject.** One horizontal max discards the whole window if any byte is non-ASCII:

```rust
let raw = vld1q_u8(bytes.as_ptr().add(pos));
if vmaxvq_u8(raw) >= 0x80 { return None; }
```

**Classify — 128-entry table in four instructions.** `vqtbl4q_u8` indexes a 64-byte table and yields
zero for out-of-range indices. Two of them cover 128 entries, and because the miss produces zero the
halves just OR together — bytes below 64 wrap past the end of the high table, bytes at or above it
miss the low one:

```rust
let cls = vorrq_u8(
    vqtbl4q_u8(lo, raw),
    vqtbl4q_u8(hi, vsubq_u8(raw, vdupq_n_u8(64))),
);
```

**Neighbouring classes — one instruction each.** This is where the DFA's advantage evaporates.
`vextq_u8` concatenates two vectors and slices a 16-byte window out of the result, so "the class of
the previous character, for all sixteen lanes at once" is a single instruction:

```rust
let prev     = vextq_u8::<15>(vsetq_lane_u8::<15>(p, vdupq_n_u8(0)), cls);
let prevprev = vextq_u8::<14>(/* pp in lane 14, p in lane 15 */, cls);
let next     = vextq_u8::<1>(cls, vsetq_lane_u8::<0>(n, vdupq_n_u8(0)));
```

The seeded lanes are how carried context enters: `p` and `pp` slide in from the left, `n` from the
right. What costs the DFA a deferred state and a re-examined character is a shifted register here.

**Rules — `vtstq_u8` per term.** `vtstq_u8(a, b)` yields `0xFF` where `a & b != 0`, so each class
test is one instruction. Eighteen of them, then the seven rules as ANDs and ORs, then `vmvnq_u8` to
invert "joined" into "break".

**Reduce to bitmasks.** NEON has no `movemask`, so:

```rust
const BITS: [u8; 16] = [1,2,4,8,16,32,64,128, 1,2,4,8,16,32,64,128];
let m = vandq_u8(v, vld1q_u8(BITS.as_ptr()));
(vaddv_u8(vget_low_u8(m)) as u16) | ((vaddv_u8(vget_high_u8(m)) as u16) << 8)
```

Each lane keeps one distinct bit, then `vaddv_u8` sums eight lanes horizontally in one instruction.
Three of these — breaks, word_like, upper.

Roughly 50 instructions per 16 bytes, ~3 per byte, versus the DFA's ~7.5 *cycles* per byte.

## 5. What actually bought the speedup

Worth separating, because it's easy to credit the wrong thing:

| Change | Gain |
| --- | --- |
| Restructuring to windows (still scalar) | 1.15x |
| Replacing the scalar kernel with NEON | 3.2x on top |

The 1.15x comes from deciding separators in bulk instead of one table transition each. The rest is
the kernel. Neither would have worked alone: SIMD on the old fast path was doomed because runs
average 5 bytes, and windowing without SIMD only removes the table lookups.

The result also lands where the arithmetic predicted. At ~8 instructions/byte with IPC ~3, ~2
cycles/byte is the floor for this shape, against 7.5 measured — about 3.7x. Measured: 3.66x.

## 6. Two traps worth remembering

**The `Newline`/`LF` bug.** The first version mapped `LF | Newline` to one class bit. But **WB3 is
`CR × LF` specifically** — U+000A, not the `Newline` class — so `\r` followed by U+000B wrongly
joined. Fixed by giving `Newline` no bit at all.

**Why the tests missed it.** Every one of the 1944 conformance cases is shorter than one 16-byte
window, so the windowed path *never ran* during them. Padding each case with ASCII at eleven
different lengths — shifting it to every alignment within a window — found the bug immediately. The
scalar-vs-NEON cross-check couldn't have: both implement the same wrong rule.

The lesson generalises. A differential test only tests the inputs you give it, and an accelerator
that declines to engage silently passes everything.

## 7. Where to look

| File | Contents |
| --- | --- |
| `src/uax29/word/ascii_window.rs` | class table, `is_break`, `defers`, both kernels, cross-check |
| `src/uax29/word/mod.rs` | `tokenize_windowed`, `state_after_ascii`, `lane_mask`, wrappers, tests |
| `benches/wikipedia.rs` | `word break`, `word break (windowed)`, `word break (windowed simd)` |

## 8. Not done yet

- **`WINDOW = 32`** using two registers, halving per-window overhead.
- **The drain is still scalar** — 23.4M `trailing_zeros` iterations, now a large share of the total.
- **aarch64 only.** An SSE2/AVX2 kernel would need `_mm_shuffle_epi8` (16-entry nibble tables rather
  than `vqtbl4q_u8`'s 64) and `_mm_movemask_epi8`, which is *cheaper* than the NEON reduction.
- **The callback costs ~0.77 ns/boundary.** At 23.4M boundaries it's now a visible fraction.

`Decisions` is the seam for all four.
