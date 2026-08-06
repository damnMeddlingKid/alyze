//! Batched UAX #29 word-break decisions for windows of pure ASCII.
//!
//! Within a run of ASCII there are no Extend, Format or ZWJ characters, so WB4 — which makes those
//! transparent and forces the DFA to remember what preceded them — cannot apply. Neither can the
//! Hebrew_Letter, Katakana, Regional_Indicator or Extended_Pictographic rules, since none of those
//! properties occur below U+0080. What is left is decidable from four adjacent character classes
//! with no carried state, which is what lets a whole window of boundaries be computed at once
//! instead of one character at a time through the state machine.
//!
//! The classes are packed one bit each so a single byte per character answers every rule.

use super::properties::{ASCII_WORD_BREAK_PROP, WordBreakProperty};

/// ALetter | Numeric | ExtendNumLet. Every ordered pair drawn from this set joins, via WB5, WB8,
/// WB9, WB10, WB13a and WB13b — so one bit covers six rules.
pub(crate) const CLS_W: u8 = 1 << 0;
pub(crate) const CLS_AL: u8 = 1 << 1;
pub(crate) const CLS_NU: u8 = 1 << 2;
/// WSegSpace. In ASCII this is U+0020 alone: tab is Word_Break=Other.
pub(crate) const CLS_SP: u8 = 1 << 3;
pub(crate) const CLS_CR: u8 = 1 << 4;
pub(crate) const CLS_LF: u8 = 1 << 5;
/// MidLetter | MidNumLet | Single_Quote — the characters WB6/WB7 bridge between letters (`:.'`).
pub(crate) const CLS_MIDAL: u8 = 1 << 6;
/// MidNum | MidNumLet | Single_Quote — the characters WB11/WB12 bridge between digits (`,;.'`).
pub(crate) const CLS_MIDNU: u8 = 1 << 7;

/// Derived from [`ASCII_WORD_BREAK_PROP`] rather than restated, so the two cannot drift.
const fn class_of(b: u8) -> u8 {
    match ASCII_WORD_BREAK_PROP[b as usize] {
        WordBreakProperty::ALetter => CLS_W | CLS_AL,
        WordBreakProperty::Numeric => CLS_W | CLS_NU,
        WordBreakProperty::ExtendNumLet => CLS_W,
        WordBreakProperty::WSegSpace => CLS_SP,
        WordBreakProperty::CR => CLS_CR,
        // WB3 is `CR × LF` specifically, so U+000A gets a bit of its own. Newline (U+000B/U+000C)
        // deliberately gets none: nothing joins to it, and no bits already means "always break",
        // which is exactly WB3a/WB3b.
        WordBreakProperty::LF => CLS_LF,
        WordBreakProperty::MidLetter => CLS_MIDAL,
        WordBreakProperty::MidNum => CLS_MIDNU,
        WordBreakProperty::MidNumLet | WordBreakProperty::SingleQuote => CLS_MIDAL | CLS_MIDNU,
        // Double_Quote only matters via WB7b/WB7c, which require Hebrew_Letter on both sides and
        // so cannot fire inside ASCII. It breaks on both sides here, like Other.
        _ => 0,
    }
}

pub(crate) const ASCII_CLASS: [u8; 128] = {
    let mut t = [0u8; 128];
    let mut i = 0u8;
    loop {
        t[i as usize] = class_of(i);
        if i == 127 {
            break;
        }
        i += 1;
    }
    t
};

/// Bytes decided per window. Sixteen so the scalar and NEON implementations agree lane for lane.
pub(crate) const WINDOW: usize = 16;

/// Is there a boundary between the characters classified `p` and `c`?
///
/// `pp` is the class two characters back and `n` the class one ahead; both are `0` (matching
/// nothing) where no such character exists, which is correct at the edges of the text because
/// sot/eot always break.
#[inline(always)]
pub(crate) const fn is_break(pp: u8, p: u8, c: u8, n: u8) -> bool {
    let joined = (p & c & CLS_W != 0)                                    // WB5/8/9/10/13a/13b
        | (p & c & CLS_SP != 0)                                          // WB3d
        | (p & CLS_CR != 0) & (c & CLS_LF != 0)                          // WB3
        | (p & CLS_AL != 0) & (c & CLS_MIDAL != 0) & (n & CLS_AL != 0)   // WB6
        | (p & CLS_MIDAL != 0) & (c & CLS_AL != 0) & (pp & CLS_AL != 0)  // WB7
        | (p & CLS_NU != 0) & (c & CLS_MIDNU != 0) & (n & CLS_NU != 0)   // WB12
        | (p & CLS_MIDNU != 0) & (c & CLS_NU != 0) & (pp & CLS_NU != 0); // WB11
    !joined
}

/// Would the state machine be mid-lookahead after consuming a character of class `c` that follows
/// one of class `p`?
///
/// WB6/WB7 and WB11/WB12 are the only ASCII rules needing a character the window may not contain,
/// so a window must not end on one: the scalar DFA would have to resume in a deferred state, which
/// it cannot do without also being handed the pending breakpoint. Windows ending here are shortened
/// by one byte instead, which always resolves because the byte before a bridge is a letter or digit.
#[inline(always)]
pub(crate) const fn defers(p: u8, c: u8) -> bool {
    ((p & CLS_AL != 0) & (c & CLS_MIDAL != 0)) | ((p & CLS_NU != 0) & (c & CLS_MIDNU != 0))
}

/// One window's worth of decisions. Every field is indexed by lane, i.e. bit `i` describes the byte
/// at `pos + i`.
pub(crate) struct Decisions {
    /// Bit `i`: a boundary falls between `pos + i - 1` and `pos + i`.
    pub breaks: u16,
    /// Bit `i`: the byte is `[a-zA-Z0-9]`, contributing `WORD_LIKE`.
    pub word_like: u16,
    /// Bit `i`: the byte is `[A-Z]`, contributing `HAS_ASCII_UPPER`.
    pub upper: u16,
    /// Class of the last byte in the window, to test [`defers`] on the handoff.
    pub last_class: u8,
    /// Class of the second-to-last byte, likewise.
    pub prev_last_class: u8,
    /// The last two bytes themselves. The handoff needs the Word_Break property rather than the
    /// window class — classes conflate everything that always breaks — and both are already in a
    /// register here, so returning them avoids reloading from the slice.
    pub last_byte: u8,
    pub prev_last_byte: u8,
}

/// Decide a window of [`WINDOW`] bytes starting at `pos`, or `None` if any of them is non-ASCII.
///
/// `pp` and `p` are the classes of the two bytes preceding `pos`; the caller must have confirmed
/// they are ASCII. `n` is the class of the byte at `pos + WINDOW`, or `0` at end of text.
#[inline]
pub(crate) fn decide_window(bytes: &[u8], pos: usize, pp: u8, p: u8, n: u8) -> Option<Decisions> {
    let window: &[u8; WINDOW] = bytes.get(pos..pos + WINDOW)?.try_into().ok()?;

    let mut classes = [0u8; WINDOW];
    for (i, &b) in window.iter().enumerate() {
        if b >= 0x80 {
            return None;
        }
        classes[i] = ASCII_CLASS[b as usize];
    }

    let mut breaks = 0u16;
    let mut word_like = 0u16;
    let mut upper = 0u16;
    for i in 0..WINDOW {
        let c_pp = if i >= 2 { classes[i - 2] } else if i == 1 { p } else { pp };
        let c_p = if i >= 1 { classes[i - 1] } else { p };
        let c_n = if i + 1 < WINDOW { classes[i + 1] } else { n };

        breaks |= (is_break(c_pp, c_p, classes[i], c_n) as u16) << i;
        word_like |= ((classes[i] & (CLS_AL | CLS_NU) != 0) as u16) << i;
        upper |= (window[i].is_ascii_uppercase() as u16) << i;
    }

    Some(Decisions {
        breaks,
        word_like,
        upper,
        last_class: classes[WINDOW - 1],
        prev_last_class: classes[WINDOW - 2],
        last_byte: window[WINDOW - 1],
        prev_last_byte: window[WINDOW - 2],
    })
}

/// NEON kernel: the same rules as [`decide_window`], evaluated lane-wise across the whole window,
/// with three horizontal reductions at the end to hand back bitmasks.
///
/// The shifted class vectors are what make the lookahead rules cheap — one `vextq_u8` produces "the
/// class of the previous character" for all sixteen lanes at once, where the state machine has to
/// carry it across iterations.
#[cfg(target_arch = "aarch64")]
#[inline]
pub(crate) fn decide_window_neon(
    bytes: &[u8],
    pos: usize,
    pp: u8,
    p: u8,
    n: u8,
) -> Option<Decisions> {
    use std::arch::aarch64::*;

    if pos + WINDOW > bytes.len() {
        return None;
    }

    /// Gather one bit per lane from a vector of 0x00/0xFF lanes.
    #[inline(always)]
    unsafe fn movemask(v: uint8x16_t) -> u16 {
        const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        unsafe {
            let m = vandq_u8(v, vld1q_u8(BITS.as_ptr()));
            (vaddv_u8(vget_low_u8(m)) as u16) | ((vaddv_u8(vget_high_u8(m)) as u16) << 8)
        }
    }

    unsafe {
        let raw = vld1q_u8(bytes.as_ptr().add(pos));
        // One horizontal max rejects the window if any byte is non-ASCII.
        if vmaxvq_u8(raw) >= 0x80 {
            return None;
        }

        // 128-entry class table as two 64-byte halves. `vqtbl4q_u8` yields zero for indices past
        // its 64-byte range, so the two halves can simply be OR'd: bytes below 64 miss the high
        // table (their index wraps well past the end) and bytes at or above it miss the low one.
        let t = ASCII_CLASS.as_ptr();
        let lo = uint8x16x4_t(
            vld1q_u8(t),
            vld1q_u8(t.add(16)),
            vld1q_u8(t.add(32)),
            vld1q_u8(t.add(48)),
        );
        let hi = uint8x16x4_t(
            vld1q_u8(t.add(64)),
            vld1q_u8(t.add(80)),
            vld1q_u8(t.add(96)),
            vld1q_u8(t.add(112)),
        );
        let cls = vorrq_u8(
            vqtbl4q_u8(lo, raw),
            vqtbl4q_u8(hi, vsubq_u8(raw, vdupq_n_u8(64))),
        );

        // Neighbouring classes for every lane at once.
        let prev = vextq_u8::<15>(vsetq_lane_u8::<15>(p, vdupq_n_u8(0)), cls);
        let prevprev = vextq_u8::<14>(
            vsetq_lane_u8::<15>(p, vsetq_lane_u8::<14>(pp, vdupq_n_u8(0))),
            cls,
        );
        let next = vextq_u8::<1>(cls, vsetq_lane_u8::<0>(n, vdupq_n_u8(0)));

        let w_c = vtstq_u8(cls, vdupq_n_u8(CLS_W));
        let w_p = vtstq_u8(prev, vdupq_n_u8(CLS_W));
        let sp_c = vtstq_u8(cls, vdupq_n_u8(CLS_SP));
        let sp_p = vtstq_u8(prev, vdupq_n_u8(CLS_SP));
        let lf_c = vtstq_u8(cls, vdupq_n_u8(CLS_LF));
        let cr_p = vtstq_u8(prev, vdupq_n_u8(CLS_CR));
        let al_c = vtstq_u8(cls, vdupq_n_u8(CLS_AL));
        let al_p = vtstq_u8(prev, vdupq_n_u8(CLS_AL));
        let al_n = vtstq_u8(next, vdupq_n_u8(CLS_AL));
        let al_pp = vtstq_u8(prevprev, vdupq_n_u8(CLS_AL));
        let nu_c = vtstq_u8(cls, vdupq_n_u8(CLS_NU));
        let nu_p = vtstq_u8(prev, vdupq_n_u8(CLS_NU));
        let nu_n = vtstq_u8(next, vdupq_n_u8(CLS_NU));
        let nu_pp = vtstq_u8(prevprev, vdupq_n_u8(CLS_NU));
        let midal_c = vtstq_u8(cls, vdupq_n_u8(CLS_MIDAL));
        let midal_p = vtstq_u8(prev, vdupq_n_u8(CLS_MIDAL));
        let midnu_c = vtstq_u8(cls, vdupq_n_u8(CLS_MIDNU));
        let midnu_p = vtstq_u8(prev, vdupq_n_u8(CLS_MIDNU));

        // The seven no-break rules, in the same order as `is_break`.
        let joined = vorrq_u8(
            vorrq_u8(
                vorrq_u8(vandq_u8(w_p, w_c), vandq_u8(sp_p, sp_c)),
                vorrq_u8(
                    vandq_u8(cr_p, lf_c),
                    vandq_u8(vandq_u8(al_p, midal_c), al_n),
                ),
            ),
            vorrq_u8(
                vorrq_u8(
                    vandq_u8(vandq_u8(midal_p, al_c), al_pp),
                    vandq_u8(vandq_u8(nu_p, midnu_c), nu_n),
                ),
                vandq_u8(vandq_u8(midnu_p, nu_c), nu_pp),
            ),
        );

        let upper = vandq_u8(
            vcgeq_u8(raw, vdupq_n_u8(b'A')),
            vcleq_u8(raw, vdupq_n_u8(b'Z')),
        );

        Some(Decisions {
            breaks: movemask(vmvnq_u8(joined)),
            word_like: movemask(vtstq_u8(cls, vdupq_n_u8(CLS_AL | CLS_NU))),
            upper: movemask(upper),
            last_class: vgetq_lane_u8::<15>(cls),
            prev_last_class: vgetq_lane_u8::<14>(cls),
            last_byte: vgetq_lane_u8::<15>(raw),
            prev_last_byte: vgetq_lane_u8::<14>(raw),
        })
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;

    /// The kernel must reproduce [`decide_window`] exactly, for every window offset and every
    /// combination of carried context.
    #[test]
    fn neon_matches_scalar() {
        let alphabet: &[u8] = b"abzAZ09_ .,;:'\"\r\n\t-/\x0b\x0c";
        let mut corpus: Vec<Vec<u8>> = Vec::new();
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..300 {
            let mut v = Vec::with_capacity(48);
            for _ in 0..48 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                v.push(alphabet[(seed % alphabet.len() as u64) as usize]);
            }
            corpus.push(v);
        }
        corpus.push(b"don't stop 3,000.50 a:b:c snake_case A.B.C   xyz".to_vec());
        // A window containing non-ASCII must be rejected by both.
        corpus.push("aaaaaaaaÉaaaaaaaaaaaaaaaa".as_bytes().to_vec());

        let ctx = [0u8, CLS_W | CLS_AL, CLS_W | CLS_NU, CLS_MIDAL, CLS_SP, CLS_CR];
        for bytes in &corpus {
            for pos in 0..=(bytes.len() - WINDOW) {
                for &pp in &ctx {
                    for &p in &ctx {
                        for &n in &ctx {
                            let a = decide_window(bytes, pos, pp, p, n);
                            let b = decide_window_neon(bytes, pos, pp, p, n);
                            match (a, b) {
                                (None, None) => {}
                                (Some(a), Some(b)) => assert_eq!(
                                    (a.breaks, a.word_like, a.upper, a.last_class,
                                     a.prev_last_class, a.last_byte, a.prev_last_byte),
                                    (b.breaks, b.word_like, b.upper, b.last_class,
                                     b.prev_last_class, b.last_byte, b.prev_last_byte),
                                    "pos {pos} pp {pp:#x} p {p:#x} n {n:#x} in {:?}",
                                    String::from_utf8_lossy(&bytes[pos..pos + WINDOW]),
                                ),
                                _ => panic!("one path rejected the window, the other did not"),
                            }
                        }
                    }
                }
            }
        }
    }
}
