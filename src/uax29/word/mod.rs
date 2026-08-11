pub(crate) mod properties;
pub(crate) mod transitions;

use crate::uax29::{Action, word::properties::WordBreakProperty::DoubleQuote};
use properties::{
    ASCII_WORD_BREAK_PROP, WordBreakProperty, is_word_like_strict,
    lookup_word_break_property_from_dictionary,
};
use transitions::{State, TABLE, Transition};

/// For backwards compatibility, require caller to pass in options struct.
#[derive(Default, Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Options {}

const WINDOW: usize = 16;

/// For a given span, extracts info from the DFA state to provide useful information upstream, e.g.
/// whether the span was "word-like", ascii, etc
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
pub struct TokenProperties(u8);

impl TokenProperties {
    const WORD_LIKE_MASK: u8 = 0b0000_0001;
    const NON_ASCII_MASK: u8 = 0b0000_0010;
    const HAS_ASCII_UPPER_MASK: u8 = 0b0000_0100;

    pub(crate) const NON_ASCII: Self = Self(Self::NON_ASCII_MASK);
    pub(crate) const WORD_LIKE: Self = Self(Self::WORD_LIKE_MASK);

    // A token is "word-like" if it contains any char that is:
    // - ALetter, HebrewLetter, or Numeric (this is a fast-path from our DFA WordBreakProperty lookup)
    // - Ideographic or Extended_Pictographic (e.g. CJK chars, emoji)
    // - Other_Number general category (⑦, ², ¼)
    // - A character whose Script is something meaningful (e.g. belonging to a real writing system),
    //   as opposed to Script=Common/Inherited/Unknown (e.g. punctuation, symbols, emoji modifiers).
    pub fn is_word_like(&self) -> bool {
        self.0 & Self::WORD_LIKE_MASK != 0
    }

    // Stored disjunctively: a single non-ASCII char in the span sets this bit.
    // `is_ascii()` returns true when the bit is unset (vacuously true for the empty span).
    pub fn is_ascii(&self) -> bool {
        self.0 & Self::NON_ASCII_MASK == 0
    }

    // Stored disjunctively: a single ASCII uppercase byte (A–Z) in the span sets this bit.
    // `has_ascii_upper()` returns true when the bit is set (vacuously false for the empty span).
    pub fn has_ascii_upper(&self) -> bool {
        self.0 & Self::HAS_ASCII_UPPER_MASK != 0
    }
}

impl std::ops::BitOrAssign for TokenProperties {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

// All the ascii break rules
// 
#[inline]
pub fn ascii_is_break(
    previous2: WordBreakProperty, 
    previous1: WordBreakProperty, 
    current:WordBreakProperty, 
    next: WordBreakProperty
) -> bool {
    // We will check all non breaking rules and then invert that so we break in all other cases
    // All non breaking rules that only have ascii
    // WB3 - Do not break within CRLF. CR × LF
    // WB3d - Keep horizontal whitespace together.
    // WB5 - Do not break between most letters. AHLetter × AHLetter
    // Do not break letters across certain punctuation, such as within “e.g.” or “example.com”.
    // WB6 - AHLetter × (MidLetter | MidNumLetQ) AHLetter
    // WB7 - AHLetter (MidLetter | MidNumLetQ) × AHLetter
    // Do not break within sequences of digits, or digits adjacent to letters (“3a”, or “A3”).
    // WB8 - Numeric × Numeric
    // WB9 - AHLetter ×	Numeric
    // WB10 - Numeric ×	AHLetter
    // Do not break within sequences, such as “3.2” or “3,456.789”.
    // WB11 - Numeric (MidNum | MidNumLetQ)	×	Numeric
    // WB12 - Numeric	×	(MidNum | MidNumLetQ) Numeric
    // Do not break from extenders.
    // WB13a - (AHLetter | Numeric | ExtendNumLet) × ExtendNumLet
    // WB13b - ExtendNumLet	× (AHLetter | Numeric)
    use WordBreakProperty::{CR, LF, ALetter, WSegSpace, MidLetter, SingleQuote, Numeric, ExtendNumLet, MidNumLet, MidNum
    };
    
    // let do_not_break = (previous1 == CR) & (current == LF) // WB3
    //     | (previous1 == WSegSpace) & (current == WSegSpace) // WB3d
    //     | matches!(previous1, ALetter) & matches!(current, ALetter) // WB5
    //     | (matches!(previous1, ALetter)
    //     & (matches!(current, MidLetter | MidNumLet | SingleQuote))
    //     & matches!(next, ALetter)) // WB6
    //     | (matches!(previous2, ALetter) 
    //     & (matches!(previous1, MidLetter | MidNumLet | SingleQuote))
    //     & matches!(current, ALetter)) // WB7
    //     | (previous1 == Numeric) & (current == Numeric) // WB8
    //     | matches!(previous1, ALetter) & (current == Numeric) // WB9
    //     | (previous1 == Numeric) & matches!(current, ALetter) // WB10
    //     | ((previous2 == Numeric) 
    //     & matches!(previous1, MidNum | MidNumLet | SingleQuote)
    //     & (current == Numeric)) // WB11
    //     | ((previous1 == Numeric) 
    //     & matches!(current, MidNum | MidNumLet | SingleQuote)
    //     & (next == Numeric)) // WB12
    //     | (matches!(previous1, ALetter | Numeric | ExtendNumLet)
    //     & (current == ExtendNumLet)) // WB13a
    //     | (previous1 == ExtendNumLet) & matches!(current, ALetter | Numeric) //WB13b
    //     ;
    // !do_not_break
    let do_not_break = (previous1 == CR) & (current == LF) // WB3
        | (previous1 == WSegSpace) & (current == WSegSpace) // WB3d
        // ALetter, Numeric and ExtendNumLet never break against each other, in
        // any of the 9 orderings. Covers WB5, WB8, WB9, WB10, WB13a and WB13b.
        | (matches!(previous1, ALetter | Numeric | ExtendNumLet)
        & matches!(current, ALetter | Numeric | ExtendNumLet))
        // MidNumLetQ = MidNumLet | SingleQuote
        | ((previous1 == ALetter)
        & matches!(current, MidLetter | MidNumLet | SingleQuote)
        & (next == ALetter)) // WB6
        | ((previous1 == Numeric)
        & matches!(current, MidNum | MidNumLet | SingleQuote)
        & (next == Numeric)) // WB12
        | ((previous2 == ALetter)
        & matches!(previous1, MidLetter | MidNumLet | SingleQuote)
        & (current == ALetter)) // WB7
        | ((previous2 == Numeric)
        & matches!(previous1, MidNum | MidNumLet | SingleQuote)
        & (current == Numeric)) // WB11
        ;
    !do_not_break
}

pub struct WindowTokens {
    pub breaks: u16,
    pub word_like: u16,
    pub ascii_upper: u16
}

#[inline]
pub fn maybe_process_ascii_window(
    bytes: &[u8], 
    pos: usize
) -> Option<WindowTokens> {
    let mut word_like: u32 = 0;
    let mut ascii_upper: u32 = 0;
    let mut is_letter: u32 = 0;
    let mut is_numeric: u32 = 0;
    let mut is_mid_let: u32 = 0;
    let mut is_mid_num: u32 = 0;
    let mut is_extend: u32 = 0;
    let mut is_cr: u32 = 0;
    let mut is_lf: u32 = 0;
    let mut is_wseg: u32 = 0;

    let mut high_bit_acc: u8 = 0; 
    for i in 0..(WINDOW + 2 + 1) {
        let b = bytes[pos -2 + i];
        high_bit_acc |= b;
    }

    if high_bit_acc & 0x80 != 0 { 
        return None;
    }

    let mask = 1;

    for i in 0..(WINDOW + 2 + 1) {
        is_mid_let |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask)) << i;
        is_mid_num |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<1)) >> 1 << i;
        is_extend |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<2)) >> 2 << i;
        is_letter |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<3)) >> 3 << i;
        is_numeric |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<4)) >> 4 << i;
        is_cr |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<5)) >> 5 << i;
        is_lf |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<6)) >> 6 << i;
        is_wseg |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<7)) >> 7 << i;
        word_like |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<8)) >> 8 << i;
        ascii_upper |= ((ASCII_CUSTOM[bytes[pos -2 + i] as usize] as u32) & (mask<<9)) >> 9 << i;
    }

    /*
     | ((previous1 == ALetter)
     & matches!(current, MidLetter | MidNumLet | SingleQuote)
     & (next == ALetter)) // WB6
     | ((previous2 == ALetter)
     & matches!(previous1, MidLetter | MidNumLet | SingleQuote)
     & (current == ALetter)) // WB7
     */

    let wb6 = (is_letter << 1) & is_mid_let & (is_letter >> 1); 
    let wb6wb7 = wb6 | (wb6 << 1);

     /*
     | ((previous1 == Numeric)
     & matches!(current, MidNum | MidNumLet | SingleQuote)
     & (next == Numeric)) // WB12
     | ((previous2 == Numeric)
     & matches!(previous1, MidNum | MidNumLet | SingleQuote)
     & (current == Numeric)) // WB11
      */
    let wb12 = (is_numeric << 1) & is_mid_num & (is_numeric >> 1);
    let wb11wb12 = wb12 | (wb12 << 1);

    /*
    | (matches!(previous1, ALetter | Numeric | ExtendNumLet)
    & matches!(current, ALetter | Numeric | ExtendNumLet))
     */
    let wbex1 = is_extend & (is_extend << 1);

    /*
    (previous1 == CR) & (current == LF) // WB3
    | (previous1 == WSegSpace) & (current == WSegSpace) // WB3d
     */
    let wbcrlf = is_lf & (is_cr << 1);
    let wbwseg = is_wseg & (is_wseg << 1);

    let do_not_break = wb6wb7 | wb11wb12 | wbex1 | wbcrlf | wbwseg;
    let breaks = !(do_not_break >> 2) as u16;

    Some(WindowTokens{breaks:breaks, word_like: (word_like >> 2) as u16, ascii_upper: (ascii_upper >> 2) as u16})
}

pub fn tokenize_windowed(
    text: &str,
    _options: Options,
    mut on_breakpoint: impl FnMut(usize, TokenProperties) -> bool,
) {
    if text.is_empty() {
        return;
    }
    let bytes = text.as_bytes();

    let mut state = State::StartOfText;
    let mut deferred_break_pos = None;
    let mut pos = 0;

    let mut last_was_zwj = false;
    let mut token_props = TokenProperties::default();
    let mut deferred_props = TokenProperties::default();
    
    while pos < text.len() {
        // ACII Windowed fast path
        // We only accept all ascii windows
        // We dont handoff state from the scalar parsing to the window
        if pos >= 2 
        && pos + WINDOW + 3 < bytes.len()
        && deferred_break_pos.is_none()
        && last_was_zwj == false 
        && bytes[pos-1] < 0x80
        && bytes[pos-2] < 0x80
        {
            while pos + WINDOW + 3 < bytes.len() {
                if let Some(res) = maybe_process_ascii_window(bytes, pos) {
                    let mut breaks = res.breaks;
                    
                    let mut start = 0;
                    while breaks != 0 {
                        let next_break = breaks.trailing_zeros();
                        let prop_mask: u16 = (1u16 << next_break) - (1u16 << start);

                        token_props.0 |= ((res.word_like & prop_mask != 0) as u8).wrapping_neg() & TokenProperties::WORD_LIKE_MASK;
                        token_props.0 |= (((res.ascii_upper & prop_mask != 0) as u8)).wrapping_neg() & TokenProperties::HAS_ASCII_UPPER_MASK;

                        if !on_breakpoint(pos + next_break as usize, std::mem::take(&mut token_props)) {
                            return;
                        }
    
                        start = next_break;
                        breaks &= breaks - 1;
                    }
    
                    // handoff to the next window, we will need to handoff tokenprops
                    // Process the next window
    
                    // handoof token props to continue into the next window
                    // we already zero'd out other tokens so theres no mask required               
                    let prop_mask: u16 =((1u32 << 16) - (1u32 << start)) as u16;
                    token_props.0 = ((res.word_like & prop_mask != 0) as u8).wrapping_neg() & TokenProperties::WORD_LIKE_MASK;
                    token_props.0 |= ((res.ascii_upper & prop_mask != 0) as u8).wrapping_neg() & TokenProperties::HAS_ASCII_UPPER_MASK;
    
                    pos += WINDOW;
                } else {
                    break;
                }
            }
        }

        // Handle unicode & Scalar runs, scalar runs happen at the edges between unicode and ascii
        if matches!(
            state,
            State::ALetter | State::Numeric | State::ExtendNumLet | State::HLetter
        ) {
            let scan_start = pos;
            let mut fast_acc: u8 = 0;
            while pos < text.len() && bytes[pos] < 0x80 {
                let info = ASCII_BYTE_INFO[bytes[pos] as usize];
                if info & ASCII_WORD_CONTINUE == 0 {
                    break;
                }
                fast_acc |= info;
                pos += 1;
            }
            if pos > scan_start {
                token_props.0 |= fast_acc & !ASCII_WORD_CONTINUE;
                let last = bytes[pos - 1]; // Safe because we're not in State::StartOfText.
                state = match last {
                    b'0'..=b'9' => State::Numeric,
                    b'_' => State::ExtendNumLet,
                    _ => State::ALetter,
                };
                last_was_zwj = false;
                continue;
            }
        }

        // Fast path for ASCII, e.g. avoid chars().next(), and lookup word property from table.
        // `char_props` is this char's contribution to the enclosing token's properties; it's
        // applied to `token_props` per-arm below, since `Action::Break` treats the breaking char
        // as the first char of the *next* token (the contribution lands there, not in the token
        // being emitted).
        let b = bytes[pos];
        let (c, prop, char_len, char_props) = if b < 0x80 {
            (
                b as char,
                ASCII_WORD_BREAK_PROP[b as usize],
                1usize,
                TokenProperties(ASCII_BYTE_INFO[b as usize] & !ASCII_WORD_CONTINUE),
            )
        } else {
            let c = text[pos..].chars().next().unwrap();
            let prop = lookup_word_break_property_from_dictionary(c);
            // Cheap path covers ALetter / HebrewLetter / Numeric. For everything else, fall back
            // to the strict per-char check (ExtPict / Ideographic / Script / OtherNumber).
            let mut char_props = TokenProperties::NON_ASCII;
            char_props |= WORD_BREAK_CONTRIB[prop as usize];
            if !char_props.is_word_like() && is_word_like_strict(c) {
                char_props |= TokenProperties::WORD_LIKE;
            }
            (c, prop, c.len_utf8(), char_props)
        };

        // Each iteration, we consult the transition table to determine the next state
        // and whether to emit a breakpoint.
        let Transition(next_state, action) = TABLE[state as usize][prop as usize];
        match action {
            Action::Break => {
                let boundary = pos;
                pos += char_len;
                if last_was_zwj {
                    last_was_zwj = false;
                    if WordBreakProperty::is_ext_pictographic(c) {
                        // Transparent: char joins the in-progress token instead of breaking.
                        token_props |= char_props;
                        continue;
                    }
                }
                last_was_zwj = prop == WordBreakProperty::ZWJ;
                state = next_state;
                if !on_breakpoint(boundary, std::mem::take(&mut token_props)) {
                    return;
                }
                // Breaking char starts the next token; apply its contribution after the take.
                token_props |= char_props;
                continue;
            }
            Action::NoBreak => {
                last_was_zwj = false;
                if next_state.is_deferred() {
                    if deferred_break_pos.is_none() {
                        deferred_break_pos = Some(pos);
                    }
                    deferred_props |= char_props;
                } else {
                    if deferred_break_pos.take().is_some() {
                        // Word resumed: deferred chars belong to the in-progress token.
                        token_props |= std::mem::take(&mut deferred_props);
                    }
                    token_props |= char_props;
                }
                state = next_state;
                pos += char_len;
            }
            Action::DeferredBreak => {
                last_was_zwj = false;
                let boundary = deferred_break_pos.take().unwrap();
                state = next_state;
                // Notably, we don't advance `pos` here; the current char is re-examined on the
                // next iteration and will accumulate its props then — don't apply char_props here.
                if !on_breakpoint(boundary, std::mem::take(&mut token_props)) {
                    return;
                }
                // Deferred chars start the next token.
                token_props |= std::mem::take(&mut deferred_props);
                continue;
            }
            Action::Transparent => {
                last_was_zwj = prop == WordBreakProperty::ZWJ;
                // State doesn't change, but we still consume the character.
                pos += char_len;
                if deferred_break_pos.is_some() {
                    deferred_props |= char_props;
                } else {
                    token_props |= char_props;
                }
            }
        }
    }

    // Deferred state at EOT - defer failed
    if state.is_deferred() {
        let breakpoint = deferred_break_pos.take().unwrap();
        if !on_breakpoint(breakpoint, std::mem::take(&mut token_props)) {
            return;
        }
        // Deferred chars become the trailing token.
        token_props |= std::mem::take(&mut deferred_props);
    }

    // WB2: Any ÷ eot — emit final segment
    _ = on_breakpoint(text.len(), token_props);
}

/// A tokenizer that implements UAX #29 word boundary rules, using a deterministic finite automaton
/// (DFA) to efficiently determine word boundaries in Unicode text. Includes a number of fast-paths
/// for common cases, e.g. ASCII.
pub fn tokenize(
    text: &str,
    _options: Options,
    mut on_breakpoint: impl FnMut(usize, TokenProperties) -> bool,
) {
    if text.is_empty() {
        return;
    }
    let bytes = text.as_bytes();

    let mut state = State::StartOfText;
    let mut deferred_break_pos = None;
    let mut pos = 0;

    // WB4 says: X (Extend | Format | ZWJ)*	→	X
    // To avoid adding _many_ `_AfterZWJ` variant states, we'll cheat a little by keeping track
    // of this condition with a bool. More specifically, we need to conditionally break based on
    // whether the previous character was a ZWJ.
    //
    // Example:
    // 'a 🛑' -> break (ALetter -> Other)
    // 'a ZWJ 🛑' -> no break (WB4)
    let mut last_was_zwj = false;

    // Maintain properties of the current token, which are reset on each break and can be used by the caller
    // to more efficiently determine what type of token was just emitted, e.g. whether it's "word-like" or ascii.
    let mut token_props = TokenProperties::default();

    // Properties of chars consumed while in a deferred state. Held aside from `token_props`
    // because we don't yet know which token they belong to: if the deferred state resolves
    // via `DeferredBreak`, these chars start the *next* token (so their contribution must
    // not leak into the in-progress one); if it resolves via `NoBreak` exiting deferred,
    // they fold into the current token. Tracked by `deferred_break_pos.is_some()`.
    let mut deferred_props = TokenProperties::default();

    while pos < text.len() {
        // Fast path for ASCII, e.g. skip DFA all together when possible.
        // Roughly a ~2x speedup on English Wikipedia.
        if matches!(
            state,
            State::ALetter | State::Numeric | State::ExtendNumLet | State::HLetter
        ) {
            let scan_start = pos;
            let mut fast_acc: u8 = 0;
            while pos < text.len() && bytes[pos] < 0x80 {
                let info = ASCII_BYTE_INFO[bytes[pos] as usize];
                if info & ASCII_WORD_CONTINUE == 0 {
                    break;
                }
                fast_acc |= info;
                pos += 1;
            }
            if pos > scan_start {
                token_props.0 |= fast_acc & !ASCII_WORD_CONTINUE;
                let last = bytes[pos - 1]; // Safe because we're not in State::StartOfText.
                state = match last {
                    b'0'..=b'9' => State::Numeric,
                    b'_' => State::ExtendNumLet,
                    _ => State::ALetter,
                };
                last_was_zwj = false;
                continue;
            }
        }

        // Fast path for ASCII, e.g. avoid chars().next(), and lookup word property from table.
        // `char_props` is this char's contribution to the enclosing token's properties; it's
        // applied to `token_props` per-arm below, since `Action::Break` treats the breaking char
        // as the first char of the *next* token (the contribution lands there, not in the token
        // being emitted).
        let b = bytes[pos];
        let (c, prop, char_len, char_props) = if b < 0x80 {
            (
                b as char,
                ASCII_WORD_BREAK_PROP[b as usize],
                1usize,
                TokenProperties(ASCII_BYTE_INFO[b as usize] & !ASCII_WORD_CONTINUE),
            )
        } else {
            let c = text[pos..].chars().next().unwrap();
            let prop = lookup_word_break_property_from_dictionary(c);
            // Cheap path covers ALetter / HebrewLetter / Numeric. For everything else, fall back
            // to the strict per-char check (ExtPict / Ideographic / Script / OtherNumber).
            let mut char_props = TokenProperties::NON_ASCII;
            char_props |= WORD_BREAK_CONTRIB[prop as usize];
            if !char_props.is_word_like() && is_word_like_strict(c) {
                char_props |= TokenProperties::WORD_LIKE;
            }
            (c, prop, c.len_utf8(), char_props)
        };

        // Each iteration, we consult the transition table to determine the next state
        // and whether to emit a breakpoint.
        let Transition(next_state, action) = TABLE[state as usize][prop as usize];
        match action {
            Action::Break => {
                let boundary = pos;
                pos += char_len;
                if last_was_zwj {
                    last_was_zwj = false;
                    if WordBreakProperty::is_ext_pictographic(c) {
                        // Transparent: char joins the in-progress token instead of breaking.
                        token_props |= char_props;
                        continue;
                    }
                }
                last_was_zwj = prop == WordBreakProperty::ZWJ;
                state = next_state;
                if !on_breakpoint(boundary, std::mem::take(&mut token_props)) {
                    return;
                }
                // Breaking char starts the next token; apply its contribution after the take.
                token_props |= char_props;
                continue;
            }
            Action::NoBreak => {
                last_was_zwj = false;
                if next_state.is_deferred() {
                    if deferred_break_pos.is_none() {
                        deferred_break_pos = Some(pos);
                    }
                    deferred_props |= char_props;
                } else {
                    if deferred_break_pos.take().is_some() {
                        // Word resumed: deferred chars belong to the in-progress token.
                        token_props |= std::mem::take(&mut deferred_props);
                    }
                    token_props |= char_props;
                }
                state = next_state;
                pos += char_len;
            }
            Action::DeferredBreak => {
                last_was_zwj = false;
                let boundary = deferred_break_pos.take().unwrap();
                state = next_state;
                // Notably, we don't advance `pos` here; the current char is re-examined on the
                // next iteration and will accumulate its props then — don't apply char_props here.
                if !on_breakpoint(boundary, std::mem::take(&mut token_props)) {
                    return;
                }
                // Deferred chars start the next token.
                token_props |= std::mem::take(&mut deferred_props);
                continue;
            }
            Action::Transparent => {
                last_was_zwj = prop == WordBreakProperty::ZWJ;
                // State doesn't change, but we still consume the character.
                pos += char_len;
                if deferred_break_pos.is_some() {
                    deferred_props |= char_props;
                } else {
                    token_props |= char_props;
                }
            }
        }
    }

    // Deferred state at EOT - defer failed
    if state.is_deferred() {
        let breakpoint = deferred_break_pos.take().unwrap();
        if !on_breakpoint(breakpoint, std::mem::take(&mut token_props)) {
            return;
        }
        // Deferred chars become the trailing token.
        token_props |= std::mem::take(&mut deferred_props);
    }

    // WB2: Any ÷ eot — emit final segment
    _ = on_breakpoint(text.len(), token_props);
}

/// Cheap-path `TokenProperties` contribution for each `WordBreakProperty` value. Covers the
/// signals that fall out of WordBreak alone — letters and digits. Katakana is intentionally
/// **not** included: its set mixes Katakana letters (word-like) with the prolonged-sound mark
/// `ー` (Script=Common, not word-like). Those split is resolved via `is_word_like_strict`.
const WORD_BREAK_CONTRIB: [TokenProperties; WordBreakProperty::NUM_VARIANTS] = {
    let mut t = [TokenProperties(0); WordBreakProperty::NUM_VARIANTS];
    t[WordBreakProperty::ALetter as usize] = TokenProperties::WORD_LIKE;
    t[WordBreakProperty::HebrewLetter as usize] = TokenProperties::WORD_LIKE;
    t[WordBreakProperty::Numeric as usize] = TokenProperties::WORD_LIKE;
    t
};

/// Per-ASCII-byte info for the fast-path scan and the single-char branch.
/// - Bit 7 (`ASCII_WORD_CONTINUE`): byte is part of a word-like run (`[a-zA-Z0-9_]`).
/// - Low bits: the byte's `TokenProperties` contribution (`WORD_LIKE_MASK` for `[a-zA-Z0-9]`,
///   since underscore continues the run but isn't itself word-like, plus
///   `HAS_ASCII_UPPER_MASK` for `[A-Z]`).
const ASCII_WORD_CONTINUE: u8 = 0b1000_0000;
const ASCII_BYTE_INFO: [u8; 128] = {
    let mut t = [0u8; 128];
    let mut i = 0u8;
    loop {
        t[i as usize] = match i {
            b'a'..=b'z' | b'0'..=b'9' => ASCII_WORD_CONTINUE | TokenProperties::WORD_LIKE_MASK,
            b'A'..=b'Z' => {
                ASCII_WORD_CONTINUE
                    | TokenProperties::WORD_LIKE_MASK
                    | TokenProperties::HAS_ASCII_UPPER_MASK
            }
            b'_' => ASCII_WORD_CONTINUE,
            _ => 0,
        };
        if i == 127 {
            break;
        }
        i += 1;
    }
    t
};

/*
is_mid_let |= (matches!(classes[i], MidLetter | MidNumLet | SingleQuote) as u32) << i;
is_mid_num |= (matches!(classes[i], MidNum | MidNumLet | SingleQuote) as u32) << i;
is_extend |= (matches!(classes[i], ALetter | Numeric | ExtendNumLet) as u32) << i;
*/
const ASCII_CUSTOM: [u16; 128] = {
    let mut t = [0u16; 128];
    let mut i = 0u16;
    loop {
        t[i as usize] = {
            /*
            is_letter |= ((classes[i] == ALetter) as u32) << i;
            is_numeric |= ((classes[i] == Numeric) as u32) << i;
            is_cr |= ((classes[i] == CR) as u32) << i;
            is_lf |= ((classes[i] == LF) as u32) << i;
            is_wseg |= ((classes[i] == WSegSpace) as u32) << i;
            
            word_like |= ((info[i] & TokenProperties::WORD_LIKE_MASK) as u32) << i;
            ascii_upper |= ((info[i] & TokenProperties::HAS_ASCII_UPPER_MASK) as u32 >> 2) << i;
             */
            let mid_let = matches!(ASCII_WORD_BREAK_PROP[i as usize], WordBreakProperty::MidLetter | WordBreakProperty::MidNumLet | WordBreakProperty::SingleQuote) as u16;
            let mid_num = matches!(ASCII_WORD_BREAK_PROP[i as usize], WordBreakProperty::MidNum | WordBreakProperty::MidNumLet | WordBreakProperty::SingleQuote) as u16;
            let extend = matches!(ASCII_WORD_BREAK_PROP[i as usize], WordBreakProperty::ALetter | WordBreakProperty::Numeric | WordBreakProperty::ExtendNumLet) as u16;
            let letter = matches!(ASCII_WORD_BREAK_PROP[i as usize], WordBreakProperty::ALetter) as u16;
            let numeric = matches!(ASCII_WORD_BREAK_PROP[i as usize], WordBreakProperty::Numeric) as u16;
            let cr = matches!(ASCII_WORD_BREAK_PROP[i as usize], WordBreakProperty::CR) as u16;
            let lf = matches!(ASCII_WORD_BREAK_PROP[i as usize], WordBreakProperty::LF) as u16;
            let wseg = matches!(ASCII_WORD_BREAK_PROP[i as usize], WordBreakProperty::WSegSpace) as u16;
            let word_like = (ASCII_BYTE_INFO[i as usize] & TokenProperties::WORD_LIKE_MASK) as u16;
            let ascii_upper = ((ASCII_BYTE_INFO[i as usize] & TokenProperties::HAS_ASCII_UPPER_MASK) >> 2) as u16;
            mid_let | (mid_num << 1) | (extend << 2) | (letter << 3) | (numeric << 4) | (cr << 5) | (lf << 6) | (wseg << 7)
            | (word_like << 8) | (ascii_upper << 9)
        };
        
        if i == 127 {
            break;
        }
        i += 1;
    }
    t
};

#[cfg(test)]
mod tests {
    use super::{Options, tokenize};
    use crate::uax29::{test_helpers::test_against_uax29_break_tests, word::tokenize_windowed};

    #[test]
    fn test_windowed_break_against_uax29_tests() {
        let (passed, failed) =
            test_against_uax29_break_tests("testdata/WordBreakTest.txt", |s, breakpoints| {
                tokenize_windowed(s, Options::default(), |bp, _props| {
                    breakpoints.push(bp);
                    true
                });
            });
        assert_eq!(
            (1944, 0),
            (passed, failed),
            "{} / {} tests passed",
            passed,
            passed + failed
        );
    }

    /// The UAX #29 corpus again, but padded with ASCII so the windowed fast path actually runs.
    ///
    /// Every case in `WordBreakTest.txt` is far shorter than one window, and the fast path needs
    /// `pos >= 2`, `pos + WINDOW < len`, and 16 consecutive ASCII bytes — so the unpadded corpus
    /// test exercises almost nothing but the scalar path. Wrapping each case in ASCII gives the
    /// window the runway it needs and slides the body through every alignment within a window.
    ///
    /// Padding changes where the boundaries fall, so the file's own annotations no longer apply;
    /// `tokenize` is the oracle instead, and `test_word_break_against_uax29_tests` pins it at
    /// 1944/1944 against the file. Breakpoints only — `tokenize_windowed` does not populate
    /// `TokenProperties` on the window path yet.
    #[test]
    fn windowed_matches_dfa_on_padded_corpus() {
        use super::WINDOW;
        use crate::uax29::test_helpers::load_break_tests;

        // Prefix lengths, chosen to land the body at every offset mod WINDOW and to straddle one,
        // two, and three window boundaries.
        const PADS: &[usize] = &[0, 1, 2, 3, 5, 8, 13, 15, 16, 17, 31, 33];
        // Long enough on its own to satisfy `pos + WINDOW < len` after the body ends.
        const TAIL: &str = " the quick brown fox jumps over it";
        const _: () = assert!(TAIL.len() > WINDOW + 2, "tail too short to reach the fast path");

        let mut mismatches = Vec::new();
        let mut total_mismatches = 0usize;
        let mut checked = 0usize;

        for case in load_break_tests("testdata/WordBreakTest.txt") {
            let body = case.codepoints_as_string();
            for &pad in PADS {
                let input = format!("{}{body}{TAIL}", "a".repeat(pad));

                let mut want = Vec::new();
                tokenize(&input, Options::default(), |bp, _props| {
                    want.push(bp);
                    true
                });
                let mut got = Vec::new();
                tokenize_windowed(&input, Options::default(), |bp, _props| {
                    got.push(bp);
                    true
                });

                checked += 1;
                if want != got {
                    total_mismatches += 1;
                    if mismatches.len() < 15 {
                        mismatches.push(format!(
                            "  pad={pad} {input:?}\n      want {want:?}\n       got {got:?}"
                        ));
                    }
                }
            }
        }

        assert!(
            mismatches.is_empty(),
            "{}\n\n{total_mismatches} / {checked} padded inputs disagree with the DFA \
             (showing first {})",
            mismatches.join("\n"),
            mismatches.len()
        );
    }

    /// A few cases from each category of `test_windowed_break_against_uax29_tests` failure, so a
    /// run points at *which kind* of input is wrong instead of just a count. Every case here is
    /// drawn from `WordBreakTest.txt` except the window-alignment group at the end, which the
    /// corpus is too short to reach — no case in it exceeds one 16-byte window.
    ///
    /// Expected breakpoints come from `tokenize`, which `test_word_break_against_uax29_tests`
    /// pins at 1944/1944 against the corpus.
    #[test]
    fn windowed_matches_dfa_on_sampled_failures() {
        // (input, expected breakpoints, category)
        const CASES: &[(&str, &[usize], &str)] = &[
            // Extend directly after ASCII mid-punctuation.
            (".\u{0308}0", &[0, 3, 4], "extend after MidNumLet"),
            (",\u{0308}A", &[0, 3, 4], "extend after MidNum"),
            (":\u{0308}_", &[0, 3, 4], "extend after MidLetter"),
            // Extend after a line terminator.
            ("\r\u{0300}", &[0, 1, 3], "extend after CR"),
            ("\n\u{0308}A", &[0, 1, 3, 4], "extend after LF"),
            ("\u{000B}\u{0300}", &[0, 1, 3], "extend after Newline"),
            // Extend after other ASCII classes.
            (" \u{0308}0", &[0, 3, 4], "extend after WSegSpace"),
            (" \u{0308}_", &[0, 3, 4], "extend after WSegSpace"),
            (" \u{0308}a:", &[0, 3, 4, 5], "extend after WSegSpace"),
            ("\0\u{0308}A", &[0, 3, 4], "extend after Other"),
            ("\0\u{0308}_", &[0, 3, 4], "extend after Other"),
            ("\0\u{0308}\u{24C2}", &[0, 3, 6], "extend after Other"),
            // Extend at start of text.
            ("\u{0300}A", &[0, 2, 3], "extend at sot"),
            ("\u{0300}_", &[0, 2, 3], "extend at sot"),
            ("\u{0300}\u{0308}0", &[0, 4, 5], "extend at sot"),
            // Format characters.
            ("\u{00AD}0", &[0, 2, 3], "format then ASCII"),
            ("\n\u{00AD}", &[0, 1, 3], "format after LF"),
            ("\u{00AD}_", &[0, 2, 3], "format then ExtendNumLet"),
            // ZWJ.
            ("\u{200D}0", &[0, 3, 4], "ZWJ then ASCII"),
            ("\r\u{200D}", &[0, 1, 4], "ZWJ after CR"),
            ("\u{000B}\u{200D}", &[0, 1, 4], "ZWJ after Newline"),
            // Regional indicators.
            ("\u{1F1E6}A", &[0, 4, 5], "RI then ASCII letter"),
            ("\u{1F1E6}_", &[0, 4, 5], "RI then ExtendNumLet"),
            ("\u{1F1E6}1:", &[0, 4, 5, 6], "RI then digit then MidLetter"),
            // Hebrew letters reached from ASCII.
            (",\u{05D0}", &[0, 1, 3], "MidNum then Hebrew"),
            ("'\u{05D0}", &[0, 1, 3], "SingleQuote then Hebrew"),
            ("\0\u{05D0}", &[0, 1, 3], "Other then Hebrew"),
            ("a\u{05D0}", &[0, 3], "ALetter then Hebrew"),
            // Katakana.
            ("\r\u{0308}\u{3031}", &[0, 1, 3, 6], "extend then Katakana"),
            ("\n\u{0308}\u{3031}", &[0, 1, 3, 6], "extend then Katakana"),
            ("\u{000B}\u{0308}\u{3031}", &[0, 1, 3, 6], "extend then Katakana"),
            // Other non-ASCII.
            ("'\u{24C2}", &[0, 1, 4], "SingleQuote then ExtPict"),
            ("\u{00A9}A", &[0, 2, 3], "Other non-ASCII then ASCII"),
            ("\u{00A9}_", &[0, 2, 3], "Other non-ASCII then ExtendNumLet"),
            // An ASCII bridge whose lookahead lands on a non-ASCII letter.
            ("a\u{0027}\u{0308}A", &[0, 5], "extend inside letter bridge"),
            ("1,\u{0308}0", &[0, 5], "extend inside numeric bridge"),
            ("a:\u{0308}A", &[0, 5], "extend inside letter bridge"),
            ("1.\u{2060}0", &[0, 6], "format inside numeric bridge"),
            ("a'\u{2060}A", &[0, 6], "format inside letter bridge"),
            ("1.\u{2060}\u{0308}0", &[0, 8], "format+extend inside bridge"),
            ("a'\u{05D0}", &[0, 4], "bridge into Hebrew"),
            ("a:\u{05D0}", &[0, 4], "bridge into Hebrew"),
            ("\u{05D0}\"\u{05D0}", &[0, 5], "Hebrew DoubleQuote bridge"),
            ("a:\u{24C2}", &[0, 5], "bridge into ExtPict"),
            ("a'\u{24C2}", &[0, 5], "bridge into ExtPict"),
            // Window alignment: the non-ASCII byte lands before, on, and after the 16-byte edge.
            ("aaaaaaaaaaaaaaa\u{0300}bbbb", &[0, 21], "non-ASCII at offset 15"),
            ("aaaaaaaaaaaaaaaa\u{0300}bbbb", &[0, 22], "non-ASCII at offset 16"),
            ("aaaaaaaaaaaaaaaaa\u{0300}bbbb", &[0, 23], "non-ASCII at offset 17"),
        ];

        let mut failures = Vec::new();
        // (category, passed, failed), in first-seen order so the tally reads like the case list.
        let mut tally: Vec<(&str, usize, usize)> = Vec::new();
        let mut passed = 0;

        for (input, expected, category) in CASES {
            let mut got = Vec::new();
            tokenize_windowed(input, Options::default(), |bp, _props| {
                got.push(bp);
                true
            });
            let ok = got == *expected;
            if ok {
                passed += 1;
            } else {
                failures.push(format!(
                    "  [{category}] {input:?}\n      want {expected:?}\n       got {got:?}"
                ));
            }
            match tally.iter_mut().find(|(c, _, _)| c == category) {
                Some(entry) => {
                    if ok {
                        entry.1 += 1;
                    } else {
                        entry.2 += 1;
                    }
                }
                None => tally.push((category, ok as usize, !ok as usize)),
            }
        }

        let failed = failures.len();
        let width = tally.iter().map(|(c, _, _)| c.len()).max().unwrap_or(0);
        let by_category: String = tally
            .iter()
            .map(|(category, pass, fail)| {
                let mark = if *fail == 0 { "ok  " } else { "FAIL" };
                format!("  {mark} {category:width$}  {pass} passed, {fail} failed\n")
            })
            .collect();

        assert!(
            failures.is_empty(),
            "failures:\n{}\n\nby category:\n{by_category}\n\
             {passed} passed, {failed} failed of {} sampled cases",
            failures.join("\n"),
            CASES.len()
        );
    }




    #[test]
    fn test_word_break_against_uax29_tests() {
        let (passed, failed) =
            test_against_uax29_break_tests("testdata/WordBreakTest.txt", |s, breakpoints| {
                tokenize(s, Options::default(), |bp, _props| {
                    breakpoints.push(bp);
                    true
                });
            });
        assert_eq!(
            (1944, 0),
            (passed, failed),
            "{} / {} tests passed",
            passed,
            passed + failed
        );
    }

    #[test]
    fn tokenizer_simple_test() {
        fn assert_breaks(s: &str) {
            let mut expected = Vec::new();
            tokenize(s, Options::default(), |bp, _props| {
                expected.push(bp);
                true
            });
            let mut actual = Vec::new();
            tokenize_windowed(s, Options::default(), |bp, _props| {
                actual.push(bp);
                true
            });
            assert_eq!(actual, expected, "input: {:?}", s);
        }
        
        assert_breaks("can'");
    }
    
    #[test]
    fn tokenizer_sanity() {
        fn assert_breaks(s: &str, expected: Vec<usize>) {
            let mut breakpoints = Vec::new();
            tokenize_windowed(s, Options::default(), |bp, _props| {
                breakpoints.push(bp);
                true
            });
            assert_eq!(breakpoints, expected, "input: {:?}", s);
        }

        // Empty string yields no breakpoints.
        assert_breaks("", vec![]);

        // Non-empty strings break at the start & end.
        assert_breaks("a", vec![0, 1]);
        assert_breaks(".", vec![0, 1]);
        assert_breaks("\n", vec![0, 1]);

        // WB5: ALetter × ALetter
        assert_breaks("hello", vec![0, 5]);

        // WB8: Numeric × Numeric
        assert_breaks("123", vec![0, 3]);

        // WB9/WB10: ALetter × Numeric, Numeric × ALetter
        assert_breaks("abc123", vec![0, 6]);
        assert_breaks("123abc", vec![0, 6]);
        assert_breaks("a1b2", vec![0, 4]);

        // WB3: CR × LF (stay together)
        assert_breaks("\r\n", vec![0, 2]);
        assert_breaks("\r\n\r\n", vec![0, 2, 4]);

        // CR and LF alone break normally
        assert_breaks("\r", vec![0, 1]);
        assert_breaks("\n\n", vec![0, 1, 2]);

        // Mixed with newlines
        assert_breaks("a\r\nb", vec![0, 1, 3, 4]);
        assert_breaks("ab\r\ncd", vec![0, 2, 4, 6]);

        // Keep horizontal whitespace together (WB3d)
        assert_breaks("a   c", vec![0, 1, 4, 5]);

        // Do not break letters across certain punctuation, such as within "e.g." or "example.com".
        assert_breaks("e.g. hello", vec![0, 3, 4, 5, 10]);
        assert_breaks("example.com", vec![0, 11]);
        assert_breaks("won't", vec![0, 5]);

        // WB13a/WB13b: ExtendNumLet connects letters, numbers, katakana
        assert_breaks("a_1", vec![0, 3]);
        assert_breaks("_a", vec![0, 2]);

        // Edge cases with deferred breaks.
        assert_breaks("can'", vec![0, 3, 4]);
        assert_breaks("can' hi", vec![0, 3, 4, 5, 7]);

        // WB7a and WB6/WB7 with Hebrew_Letter and Single_Quote.
        assert_breaks("א'", vec![0, "א'".len()]);
        assert_breaks("א'א", vec![0, "א'א".len()]);
        assert_breaks("א'\u{2060}א", vec![0, "א'\u{2060}א".len()]);
        assert_breaks("א'a", vec![0, "א'a".len()]);
        assert_breaks("הצ'קרות", vec![0, "הצ'קרות".len()]);
        assert_breaks(
            "לייף אנרג'י",
            vec![0, "לייף".len(), "לייף ".len(), "לייף אנרג'י".len()],
        );

        // WB7b/WB7c: Hebrew_Letter × Double_Quote × Hebrew_Letter (gershayim acronyms
        // like צה״ל). With letters on both sides the gershayim is absorbed into the
        // word; with whitespace on either side it must emit as its own standalone
        // token (UAX #29 prescribes a break — no MidLetter/DoubleQuote rule applies).
        assert_breaks("צה\u{05F4}ל", vec![0, "צה\u{05F4}ל".len()]);
        // Closing gershayim followed by space: standalone token.
        assert_breaks(
            "אקספרס\u{05F4} מהיום",
            vec![
                0,
                "אקספרס".len(),
                "אקספרס\u{05F4}".len(),
                "אקספרס\u{05F4} ".len(),
                "אקספרס\u{05F4} מהיום".len(),
            ],
        );
        // Full quoted-word pattern: both opening and closing gershayim are standalone.
        assert_breaks(
            "\u{05F4}אקספרס\u{05F4} מהיום",
            vec![
                0,
                "\u{05F4}".len(),
                "\u{05F4}אקספרס".len(),
                "\u{05F4}אקספרס\u{05F4}".len(),
                "\u{05F4}אקספרס\u{05F4} ".len(),
                "\u{05F4}אקספרס\u{05F4} מהיום".len(),
            ],
        );

        // WB3c: ZWJ × Extended_Pictographic (emoji ZWJ sequences)
        assert_breaks("👨\u{200D}👩", vec![0, 11]);
        assert_breaks("👨👩", vec![0, 4, 8]);

        // Weird edge case: Letters that are also extended pictographic
        assert_breaks("🇦", vec![0, 4]);
        assert_breaks("🇦🇦", vec![0, 8]);
        assert_breaks("🇦🇦🇦", vec![0, 8, 12]);

        // Circled letters
        assert_breaks("\u{200d}Ⓜ", vec![0, 6]);
    }

    #[test]
    fn tokenizer_properties_sanity() {
        // Each emit reports properties of the span just closed; the leading boundary at 0 has
        // no preceding span, so it carries default props.
        fn assert_props(s: &str, expected: Vec<(usize, bool)>) {
            let mut got: Vec<(usize, bool)> = Vec::new();
            tokenize(s, Options::default(), |bp, props| {
                got.push((bp, props.is_ascii()));
                true
            });
            assert_eq!(got, expected, "input: {:?}", s);
        }

        // Leading boundary at 0 is vacuously is_ascii=true.
        assert_props("hello", vec![(0, true), (5, true)]);
        assert_props("🛑", vec![(0, true), (4, false)]);

        // The sharp case: the breaking char is non-ASCII but starts the *next* token, so "ab"
        // must still report is_ascii=true and "🛑" must report is_ascii=false.
        assert_props("ab🛑", vec![(0, true), (2, true), (6, false)]);
    }

    #[test]
    fn tokenizer_has_ascii_upper_sanity() {
        // Each emit reports properties of the span just closed; the leading boundary at 0 has
        // no preceding span, so has_ascii_upper is vacuously false.
        fn assert_has_ascii_upper(s: &str, expected: Vec<(usize, bool)>) {
            let mut got: Vec<(usize, bool)> = Vec::new();
            tokenize(s, Options::default(), |bp, props| {
                got.push((bp, props.has_ascii_upper()));
                true
            });
            assert_eq!(got, expected, "input: {:?}", s);
        }

        assert_has_ascii_upper("hello", vec![(0, false), (5, false)]);
        assert_has_ascii_upper("Hello", vec![(0, false), (5, true)]);
        assert_has_ascii_upper("HELLO", vec![(0, false), (5, true)]);
        assert_has_ascii_upper("aB", vec![(0, false), (2, true)]);
        assert_has_ascii_upper("123", vec![(0, false), (3, false)]);

        // The breaking char is non-ASCII but starts the *next* token, so "ab" must still
        // report has_ascii_upper=false.
        assert_has_ascii_upper("ab🛑", vec![(0, false), (2, false), (6, false)]);
    }

    fn assert_word_like(s: &str, expected: Vec<(usize, bool)>) {
        let mut got: Vec<(usize, bool)> = Vec::new();
        tokenize(s, Options::default(), |bp, props| {
            got.push((bp, props.is_word_like()));
            true
        });
        assert_eq!(got, expected, "input: {:?}", s);
    }

    /// ASCII subset of the word-like contract: any token containing an ASCII letter or digit is
    /// word-like; pure-connector / whitespace / punctuation tokens are not. The leading boundary
    /// at 0 has no preceding span, so word_like is vacuously false.
    #[test]
    fn tokenizer_word_like_ascii_sanity() {
        // ASCII letters / digits / mixed / contractions.
        assert_word_like("hello", vec![(0, false), (5, true)]);
        assert_word_like("123", vec![(0, false), (3, true)]);
        assert_word_like("abc123", vec![(0, false), (6, true)]);
        assert_word_like("won't", vec![(0, false), (5, true)]);

        // Connectors only (ExtendNumLet) — `_` is not a letter or digit.
        assert_word_like("___", vec![(0, false), (3, false)]);
        // Whitespace only.
        assert_word_like("   ", vec![(0, false), (3, false)]);
        // ASCII punctuation: each '!' breaks separately, none word-like.
        assert_word_like("!!!", vec![(0, false), (1, false), (2, false), (3, false)]);
    }

    /// One reduced case from `windowed_token_props_on_padded_ascii`, hardcoded.
    ///
    /// The shortest input that reproduces the property bug: 25 bytes, one window plus a tail.
    /// Breakpoints are all correct — only the `WORD_LIKE` bit on the token `"brown"` is wrong,
    /// and only that one, while the tokens before and after it are fine. `"brown"` is the token
    /// that spans the handoff out of the first window, so this pins the carry at
    /// `tokenize_windowed`'s `if last_break != 0` block rather than the per-token mask slicing.
    ///
    /// Expected values are written out rather than taken from `tokenize`, so a debugger session
    /// on this test has a fixed target that cannot move if the oracle changes.
    #[test]
    fn windowed_token_props_single_case() {
        use super::TokenProperties;

        const INPUT: &str = "hello the quick brown fox";
        // (breakpoint, raw TokenProperties bits, the token that just closed)
        const EXPECTED: &[(usize, u8, &str)] = &[
            (0, 0b000, ""),
            (5, 0b001, "hello"),
            (6, 0b000, " "),
            (9, 0b001, "the"),
            (10, 0b000, " "),
            (15, 0b001, "quick"),
            (16, 0b000, " "),
            (21, 0b001, "brown"), // <-- windowed reports 0b000 here
            (22, 0b000, " "),
            (25, 0b001, "fox"),
        ];

        let mut got: Vec<(usize, u8)> = Vec::new();
        tokenize_windowed(INPUT, Options::default(), |bp, props: TokenProperties| {
            got.push((bp, props.0));
            true
        });

        let want: Vec<(usize, u8)> = EXPECTED.iter().map(|&(bp, p, _)| (bp, p)).collect();
        if got != want {
            let mut report = String::new();
            for (i, &(bp, bits, tok)) in EXPECTED.iter().enumerate() {
                let g = got.get(i);
                let mark = if g == Some(&(bp, bits)) { "   " } else { "-> " };
                report.push_str(&format!(
                    "{mark}#{i} {tok:?}\n      want bp={bp} props={bits:#05b}\n       got {}\n",
                    match g {
                        Some((b, p)) => format!("bp={b} props={p:#05b}"),
                        None => "<no emit>".to_string(),
                    }
                ));
            }
            panic!("{INPUT:?}\n{report}");
        }
    }

    /// The bodies from `tokenizer_word_like_ascii_sanity`, padded so they run through the
    /// windowed fast path rather than the scalar one.
    ///
    /// Unpadded those cases are all shorter than one window, so they never reach
    /// `maybe_process_ascii_window` and say nothing about the property bookkeeping there — which
    /// is the interesting part, since the window accumulates `word_like` / `ascii_upper` as lane
    /// masks and has to slice them per token and carry the tail across the window handoff.
    ///
    /// Expectations come from `tokenize` rather than being written out, because padding changes
    /// both the offsets and which token each body merges into.
    #[test]
    fn windowed_token_props_on_padded_ascii() {
        // Bodies from `tokenizer_word_like_ascii_sanity`, plus uppercase and boundary-straddling
        // cases so `has_ascii_upper` and the cross-window carry are exercised too.
        const BODIES: &[&str] = &[
            "hello", "123", "abc123", "won't", "___", "   ", "!!!", "Hello", "aB", "HELLO",
            "a_b_c", "3.14", "e.g.", "x", "",
        ];
        const PADS: &[usize] = &[0, 1, 2, 3, 5, 8, 13, 15, 16, 17, 31, 33];
        const TAIL: &str = " the quick brown fox jumps over it";

        use super::TokenProperties;

        // The whole bitfield, not the three accessors — a bit that neither side exposes yet still
        // has to match, and a raw value diffs more legibly than three separate bools.
        fn run(
            tok: impl Fn(&str, Options, &mut dyn FnMut(usize, TokenProperties) -> bool),
            s: &str,
        ) -> Vec<(usize, u8)> {
            let mut out = Vec::new();
            tok(s, Options::default(), &mut |bp, props| {
                out.push((bp, props.0));
                true
            });
            out
        }

        let mut failures = Vec::new();
        let mut checked = 0usize;
        for body in BODIES {
            for &pad in PADS {
                let input = format!("{}{body}{TAIL}", "a".repeat(pad));
                let want = run(|s, o, cb| tokenize(s, o, cb), &input);
                let got = run(|s, o, cb| tokenize_windowed(s, o, cb), &input);
                checked += 1;
                if want != got {
                    let first = want
                        .iter()
                        .zip(&got)
                        .position(|(a, b)| a != b)
                        .unwrap_or(want.len().min(got.len()));
                    let fmt = |e: Option<&(usize, u8)>| match e {
                        Some((bp, bits)) => format!("bp={bp} props={bits:#07b}"),
                        None => "<no emit>".to_string(),
                    };
                    failures.push(format!(
                        "  body={body:?} pad={pad} {input:?}\n      \
                         first differing emit #{first}\n      \
                         want {}\n       got {}",
                        fmt(want.get(first)),
                        fmt(got.get(first)),
                    ));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{}\n\n{} / {checked} padded inputs disagree on token properties",
            failures.iter().take(10).cloned().collect::<Vec<_>>().join("\n"),
            failures.len(),
        );
    }

    /// Strict cases that need Script / Ideographic / OtherNumber / ExtPict lookups beyond the
    /// WordBreak property.
    #[test]
    fn tokenizer_word_like_strict_sanity() {
        // Hebrew (HebrewLetter prop)
        assert_word_like("ש", vec![(0, false), (2, true)]);

        // CJK ideograph: WordBreak=Other, Script=Han.
        assert_word_like("中", vec![(0, false), (3, true)]);
        // Ideographic iteration mark: WordBreak=Other, Script=Common, Ideographic=true.
        assert_word_like("々", vec![(0, false), (3, true)]);
        // Circled digit: WordBreak=Other, GeneralCategory=OtherNumber.
        assert_word_like("①", vec![(0, false), (3, true)]);
        // Devanagari letter: WordBreak=Other, Script=Devanagari.
        assert_word_like("अ", vec![(0, false), (3, true)]);
        // Thai letter: WordBreak=Other, Script=Thai.
        assert_word_like("ก", vec![(0, false), (3, true)]);
        // Emoji: WordBreak=Other (or ExtPict), Script=Common, ExtendedPictographic=true.
        assert_word_like("👍", vec![(0, false), (4, true)]);

        // Real Katakana letter: WordBreak=Katakana, Script=Katakana → word-like.
        assert_word_like("リ", vec![(0, false), (3, true)]);
        // Katakana-Hiragana extender: WordBreak=Katakana, Script=Common → NOT word-like.
        // Locks in why we can't just OR `WordBreakProperty::Katakana → WORD_LIKE`; we need to
        // additionally check the char's Script.
        assert_word_like("ー", vec![(0, false), (3, false)]);

        // HEBREW PUNCTUATION GERSHAYIM (U+05F4): WordBreak=DoubleQuote (not word-like via
        // cheap path), Script=Hebrew (word-like via strict). Standalone token is word-like.
        assert_word_like("\u{05F4}", vec![(0, false), ("\u{05F4}".len(), true)]);
    }

    /// A deferred break must not strand the deferred char's properties on the preceding
    /// token. For `אקספרס״ `, the closing gershayim emerges as a standalone token via
    /// `DeferredBreak` from `HLetterDQ`; its `WORD_LIKE` bit (via Script=Hebrew) belongs
    /// to that standalone token, not to the Hebrew word that precedes it.
    #[test]
    fn deferred_break_does_not_misattribute_props() {
        let s = "אקספרס\u{05F4} ";
        assert_word_like(
            s,
            vec![
                (0, false),
                ("אקספרס".len(), true),         // אקספרס (Hebrew letters)
                ("אקספרס\u{05F4}".len(), true), // ״ standalone — Script=Hebrew
                (s.len(), false),               // trailing space
            ],
        );

        // Same shape with Hebrew word after the space — the four-quote pattern from
        // real Common Crawl docs (`״אקספרס״ מהיום …`). All four standalone gershayim
        // tokens must be word-like; the asymmetry-bug case is the trailing one.
        let s = "\u{05F4}אקספרס\u{05F4} מהיום";
        assert_word_like(
            s,
            vec![
                (0, false),
                ("\u{05F4}".len(), true),                 // leading ״
                ("\u{05F4}אקספרס".len(), true),           // אקספרס
                ("\u{05F4}אקספרס\u{05F4}".len(), true),   // trailing ״
                ("\u{05F4}אקספרס\u{05F4} ".len(), false), // space
                (s.len(), true),                          // מהיום
            ],
        );
    }
}
