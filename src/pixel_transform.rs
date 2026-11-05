//! Pixel transform derived image items (`sato`, ISO/IEC 23008-12 4th
//! edition working draft §6.6.2.6): every output sample is a postfix
//! integer expression over constants and the co-located samples of
//! the `dimg` inputs — e.g. a 16-bit picture carried as two 8-bit coded
//! items (`256 × in₁ + in₂`, Annex J.9.1) or a lossy base plus a residual
//! (`16 × in₁ + in₂ − 128`, J.9.2).
//!
//! [`PixelTransform`] parses / serialises the `PixelTransform` body
//! (validating the §6.6.2.6.4 stack constraints up front, so evaluation
//! never fails) and [`PixelTransform::evaluate`] runs the expression at
//! the signalled intermediate precision, every result that leaves the
//! signed `num_bits` range saturating to its bound (§6.6.2.6.3).

use crate::error::{HeifError, Result};

/// One token of a `PixelTransform` expression (§6.6.2.6.3, Table 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PixelToken {
    /// Token 0: a literal constant (read at the intermediate precision).
    Constant(i64),
    /// Tokens 1..=32: the sample of the given 1-based `dimg` input.
    Input(u8),
    /// 64: −L.
    Negation,
    /// 65: |L|.
    Absolute,
    /// 66: bitwise complement.
    Not,
    /// 67: index of the most significant set bit (0 when L ≤ 0).
    Bsr,
    /// 128: L + R.
    Sum,
    /// 129: L − R.
    Difference,
    /// 130: L × R.
    Product,
    /// 131: L / R truncated toward zero (L when R = 0).
    Quotient,
    /// 132: L ∧ R.
    And,
    /// 133: L ∨ R.
    Or,
    /// 134: L ⊕ R.
    Xor,
    /// 135: L to the power R, truncated (0 when L = 0).
    Pow,
    /// 136: min(L, R).
    Min,
    /// 137: max(L, R).
    Max,
}

impl PixelToken {
    /// The token's code (Table 6).
    pub fn code(&self) -> u8 {
        match self {
            Self::Constant(_) => 0,
            Self::Input(i) => *i,
            Self::Negation => 64,
            Self::Absolute => 65,
            Self::Not => 66,
            Self::Bsr => 67,
            Self::Sum => 128,
            Self::Difference => 129,
            Self::Product => 130,
            Self::Quotient => 131,
            Self::And => 132,
            Self::Or => 133,
            Self::Xor => 134,
            Self::Pow => 135,
            Self::Min => 136,
            Self::Max => 137,
        }
    }

    /// The operator for a code, `None` for a reserved one (0 and 1..=32
    /// are operands).
    fn operator(code: u8) -> Option<Self> {
        Some(match code {
            64 => Self::Negation,
            65 => Self::Absolute,
            66 => Self::Not,
            67 => Self::Bsr,
            128 => Self::Sum,
            129 => Self::Difference,
            130 => Self::Product,
            131 => Self::Quotient,
            132 => Self::And,
            133 => Self::Or,
            134 => Self::Xor,
            135 => Self::Pow,
            136 => Self::Min,
            137 => Self::Max,
            _ => return None,
        })
    }

    /// Stack values popped (operands push, unary pops 1, binary 2).
    fn arity(&self) -> usize {
        match self {
            Self::Constant(_) | Self::Input(_) => 0,
            Self::Negation | Self::Absolute | Self::Not | Self::Bsr => 1,
            _ => 2,
        }
    }
}

/// A parsed `PixelTransform` body (§6.6.2.6.2).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PixelTransform {
    /// `bit_depth` (0..=3): intermediate precision `8 << bit_depth`
    /// bits, sign included (Table 5).
    pub bit_depth: u8,
    /// The tokens, in evaluation order.
    pub tokens: Vec<PixelToken>,
}

impl PixelTransform {
    /// Build a transform; the expression is validated as on parse
    /// against `input_count` inputs.
    pub fn new(bit_depth: u8, tokens: Vec<PixelToken>, input_count: usize) -> Result<Self> {
        let t = Self { bit_depth, tokens };
        t.validate(input_count)?;
        Ok(t)
    }

    /// The intermediate precision in bits (`num_bits`, Table 5).
    pub fn num_bits(&self) -> u32 {
        8 << self.bit_depth
    }

    /// The signed range of the intermediate precision.
    fn range(&self) -> (i128, i128) {
        let n = self.num_bits();
        (-(1i128 << (n - 1)), (1i128 << (n - 1)) - 1)
    }

    /// Parse a `sato` item body for an item with `input_count` `dimg`
    /// inputs. An unrecognised `version` or a reserved token is
    /// [`HeifError::Unsupported`] (§6.6.2.6.3: readers shall ignore the
    /// item); an expression breaking §6.6.2.6.4 is
    /// [`HeifError::InvalidData`] ("rejected by parsers as invalid
    /// files").
    pub fn parse(body: &[u8], input_count: usize) -> Result<Self> {
        let mut r = crate::boxes::Reader::new(body);
        let head = r.u8("sato version / bit_depth")?;
        let version = head >> 6;
        if version != 0 {
            return Err(HeifError::unsupported(format!(
                "sato: PixelTransform version {version} (readers ignore it, §6.6.2.6.3)"
            )));
        }
        let bit_depth = head & 3;
        let count = r.u8("sato token_count")? as usize;
        let bytes = 1usize << bit_depth;
        let mut tokens = Vec::with_capacity(count);
        for _ in 0..count {
            let code = r.u8("sato token")?;
            tokens.push(match code {
                0 => {
                    let raw = r.uint(bytes, "sato constant")?;
                    let shift = 64 - 8 * bytes as u32;
                    Constant(((raw << shift) as i64) >> shift)
                }
                1..=32 => PixelToken::Input(code),
                _ => PixelToken::operator(code).ok_or_else(|| {
                    HeifError::unsupported(format!(
                        "sato: reserved token {code} (readers ignore the item, §6.6.2.6.3)"
                    ))
                })?,
            });
        }
        let t = Self { bit_depth, tokens };
        t.validate(input_count)?;
        Ok(t)
    }

    /// The §6.6.2.6.4 constraints: at least one token, every input
    /// operand at most `input_count`, enough operands for every
    /// operator, exactly one value left; constants within the
    /// intermediate precision.
    fn validate(&self, input_count: usize) -> Result<()> {
        if self.bit_depth > 3 {
            return Err(HeifError::invalid(format!(
                "sato: bit_depth {}",
                self.bit_depth
            )));
        }
        if self.tokens.is_empty() || self.tokens.len() > 255 {
            return Err(HeifError::invalid(format!(
                "sato: token_count {} (1..=255, §6.6.2.6.4)",
                self.tokens.len()
            )));
        }
        let (lo, hi) = self.range();
        let mut depth = 0usize;
        for (i, t) in self.tokens.iter().enumerate() {
            match t {
                PixelToken::Input(k) if *k == 0 || *k > 32 || *k as usize > input_count => {
                    return Err(HeifError::invalid(format!(
                        "sato: token {i} reads input {k} of {input_count} (§6.6.2.6.4)"
                    )));
                }
                PixelToken::Constant(c) if (*c as i128) < lo || (*c as i128) > hi => {
                    return Err(HeifError::invalid(format!(
                        "sato: constant {c} outside {} bits",
                        self.num_bits()
                    )));
                }
                _ => {}
            }
            let a = t.arity();
            if depth < a {
                return Err(HeifError::invalid(format!(
                    "sato: token {i} needs {a} stack values, {depth} present (§6.6.2.6.4)"
                )));
            }
            depth = depth - a + 1;
        }
        if depth != 1 {
            return Err(HeifError::invalid(format!(
                "sato: {depth} values left on the stack (exactly one, §6.6.2.6.4)"
            )));
        }
        Ok(())
    }

    /// Serialise the body.
    pub fn to_bytes(&self) -> Vec<u8> {
        let bytes = 1usize << self.bit_depth;
        let mut b = vec![self.bit_depth & 3, self.tokens.len() as u8];
        for t in &self.tokens {
            b.push(t.code());
            if let PixelToken::Constant(c) = t {
                b.extend_from_slice(&c.to_be_bytes()[8 - bytes..]);
            }
        }
        b
    }

    /// The highest input index the expression reads (0 when none).
    pub fn inputs_read(&self) -> u8 {
        self.tokens
            .iter()
            .filter_map(|t| match t {
                PixelToken::Input(k) => Some(*k),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// Evaluate the expression for one sample position; `inputs[k - 1]`
    /// is the sample of input `k`. `stack` is scratch space (cleared
    /// here) so a caller evaluating many samples allocates once.
    /// Every intermediate result outside the signed `num_bits` range is
    /// replaced by the nearest bound (§6.6.2.6.3). The expression was
    /// validated, so the stack never underflows.
    pub fn evaluate(&self, inputs: &[i64], stack: &mut Vec<i64>) -> i64 {
        let (lo, hi) = self.range();
        let clamp = |v: i128| v.clamp(lo, hi) as i64;
        stack.clear();
        for t in &self.tokens {
            let v = match *t {
                PixelToken::Constant(c) => c,
                PixelToken::Input(k) => {
                    clamp(inputs.get(k as usize - 1).copied().unwrap_or(0) as i128)
                }
                PixelToken::Negation => clamp(-(pop(stack) as i128)),
                PixelToken::Absolute => clamp((pop(stack) as i128).abs()),
                PixelToken::Not => !pop(stack),
                PixelToken::Bsr => {
                    let l = pop(stack);
                    if l <= 0 {
                        0
                    } else {
                        63 - l.leading_zeros() as i64
                    }
                }
                op => {
                    let r = pop(stack) as i128;
                    let l = pop(stack) as i128;
                    match op {
                        PixelToken::Sum => clamp(l + r),
                        PixelToken::Difference => clamp(l - r),
                        PixelToken::Product => clamp(l * r),
                        PixelToken::Quotient => {
                            if r == 0 {
                                l as i64
                            } else {
                                clamp(l / r)
                            }
                        }
                        PixelToken::And => (l & r) as i64,
                        PixelToken::Or => (l | r) as i64,
                        PixelToken::Xor => (l ^ r) as i64,
                        PixelToken::Pow => clamp(power(l, r, hi)),
                        PixelToken::Min => l.min(r) as i64,
                        _ => l.max(r) as i64,
                    }
                }
            };
            stack.push(v);
        }
        stack.pop().unwrap_or(0)
    }
}

/// The reconstructed image of a `sato` item (§6.6.2.6.1): every colour
/// sample is [`PixelTransform::evaluate`] over the co-located samples
/// of `inputs` (same size, chroma layout and channel count — a
/// mismatch is [`HeifError::InvalidData`]; their alpha planes are not
/// channels of the derivation), clamped to the `bit_depth` of the
/// item's `pixi` and, when `nclx` says limited range
/// (`full_range_flag = 0`), to the nominal range of that depth —
/// `16..=235` (luma, and every channel under the identity matrix) /
/// `16..=240` (chroma), times `2^(bit_depth − 8)`. No colour
/// conversion is applied to the inputs. Depths outside 8..=16 are
/// [`HeifError::Unsupported`] (the frame type's range).
pub fn compose(
    transform: &PixelTransform,
    inputs: &[crate::image::HeifFrame],
    bit_depth: u8,
    nclx: Option<&crate::props::Colr>,
) -> Result<crate::image::HeifFrame> {
    use crate::image::{HeifFrame, HeifPixelFormat};
    let first = inputs
        .first()
        .ok_or_else(|| HeifError::invalid("sato: no input image"))?;
    if !(8..=16).contains(&bit_depth) {
        return Err(HeifError::unsupported(format!(
            "sato: output bit depth {bit_depth} (8..=16 composed)"
        )));
    }
    let chroma = first.format.chroma;
    for (k, f) in inputs.iter().enumerate() {
        if (f.width, f.height) != (first.width, first.height) || f.format.chroma != chroma {
            return Err(HeifError::invalid(format!(
                "sato: input {} is {}x{} {:?}, input 1 {}x{} {:?} (same dimensions and chroma, §6.6.2.6.1)",
                k + 1,
                f.width,
                f.height,
                f.format.chroma,
                first.width,
                first.height,
                chroma
            )));
        }
    }
    let format = HeifPixelFormat::new(chroma, bit_depth, false)?;
    let mut out = HeifFrame::zeroed(first.width, first.height, format)?;
    let max = (1i64 << bit_depth) - 1;
    let (limited, identity) = match nclx {
        Some(crate::props::Colr::Nclx {
            full_range, matrix, ..
        }) => (!*full_range, *matrix == 0),
        _ => (false, false),
    };
    let sh = 1i64 << (bit_depth - 8);
    let mut stack = Vec::with_capacity(transform.tokens.len());
    let mut samples = vec![0i64; inputs.len()];
    for p in 0..chroma.colour_planes() {
        let (lo, hi) = if !limited {
            (0, max)
        } else if p == 0 || identity {
            (16 * sh, 235 * sh)
        } else {
            (16 * sh, 240 * sh)
        };
        let (w, h) = out.plane_dims(p);
        for y in 0..h {
            for x in 0..w {
                for (s, f) in samples.iter_mut().zip(inputs) {
                    *s = f.sample(p, x, y) as i64;
                }
                let v = transform.evaluate(&samples, &mut stack).clamp(lo, hi);
                out.set_sample(p, x, y, v as u16);
            }
        }
    }
    Ok(out)
}

use PixelToken::Constant;

fn pop(stack: &mut Vec<i64>) -> i64 {
    stack.pop().unwrap_or(0)
}

/// `truncate(L^R)`, 0 when L = 0 (Table 6); a negative exponent leaves
/// only ±1 bases non-zero after truncation. A magnitude beyond `hi`
/// stops the loop at `hi + 1` with the result's sign, which the caller
/// clamps to the bound.
fn power(l: i128, r: i128, hi: i128) -> i128 {
    if l == 0 {
        return 0;
    }
    let negative = l < 0 && r & 1 == 1;
    if r < 0 {
        return match l {
            1 => 1,
            -1 => {
                if negative {
                    -1
                } else {
                    1
                }
            }
            _ => 0,
        };
    }
    let base = l.abs();
    let mut acc: i128 = 1;
    let mut e = r;
    while e > 0 {
        acc = acc.saturating_mul(base);
        if acc > hi {
            acc = hi + 1;
            break;
        }
        e -= 1;
        if base == 1 {
            break;
        }
    }
    if negative { -acc } else { acc }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(t: &PixelTransform, inputs: &[i64]) -> i64 {
        t.evaluate(inputs, &mut Vec::new())
    }

    /// Annex J.9.1 / J.9.2 examples.
    #[test]
    fn annex_examples() {
        use PixelToken::*;
        let suffix =
            PixelTransform::new(2, vec![Constant(256), Input(1), Product, Input(2), Sum], 2)
                .unwrap();
        assert_eq!(eval(&suffix, &[0xAB, 0xCD]), 0xABCD);
        let bytes = suffix.to_bytes();
        assert_eq!(bytes, [2, 5, 0, 0, 0, 1, 0, 1, 130, 2, 128]);
        assert_eq!(PixelTransform::parse(&bytes, 2).unwrap(), suffix);
        let residual = PixelTransform::new(
            2,
            vec![
                Constant(16),
                Input(1),
                Product,
                Input(2),
                Sum,
                Constant(128),
                Difference,
            ],
            2,
        )
        .unwrap();
        assert_eq!(eval(&residual, &[4095, 255]), 16 * 4095 + 127);
        assert_eq!(eval(&residual, &[0, 0]), -128);
    }

    #[test]
    fn operators_saturate_at_the_intermediate_precision() {
        use PixelToken::*;
        let t = |tokens: Vec<PixelToken>| PixelTransform::new(0, tokens, 2).unwrap();
        // 8-bit intermediate: 100 + 100 saturates to 127.
        assert_eq!(eval(&t(vec![Input(1), Input(2), Sum]), &[100, 100]), 127);
        assert_eq!(
            eval(&t(vec![Input(1), Input(2), Difference]), &[-100, 100]),
            -128
        );
        assert_eq!(eval(&t(vec![Input(1), Negation]), &[-128]), 127);
        assert_eq!(eval(&t(vec![Input(1), Absolute]), &[-128]), 127);
        assert_eq!(eval(&t(vec![Input(1), Not]), &[5]), -6);
        assert_eq!(eval(&t(vec![Input(1), Bsr]), &[100]), 6);
        assert_eq!(eval(&t(vec![Input(1), Bsr]), &[-3]), 0);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Quotient]), &[-7, 2]), -3);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Quotient]), &[7, 0]), 7);
        assert_eq!(
            eval(&t(vec![Input(1), Input(2), Quotient]), &[-128, -1]),
            127
        );
        assert_eq!(eval(&t(vec![Input(1), Input(2), Pow]), &[2, 6]), 64);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Pow]), &[2, 7]), 127);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Pow]), &[-2, 7]), -128);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Pow]), &[0, 0]), 0);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Pow]), &[5, 0]), 1);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Pow]), &[2, -1]), 0);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Pow]), &[-1, -3]), -1);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Pow]), &[1, 120]), 1);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Min]), &[3, -4]), -4);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Max]), &[3, -4]), 3);
        assert_eq!(eval(&t(vec![Input(1), Input(2), And]), &[-1, 12]), 12);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Or]), &[8, 4]), 12);
        assert_eq!(eval(&t(vec![Input(1), Input(2), Xor]), &[-1, 0]), -1);
        // 64-bit intermediate.
        let w = PixelTransform::new(3, vec![Constant(i64::MAX), Input(1), Sum], 1).unwrap();
        assert_eq!(eval(&w, &[1]), i64::MAX);
        let back = PixelTransform::parse(&w.to_bytes(), 1).unwrap();
        assert_eq!(back, w);
        // Negative 16-bit constant sign-extends.
        let n = PixelTransform::new(1, vec![Constant(-300)], 0).unwrap();
        assert_eq!(PixelTransform::parse(&n.to_bytes(), 0).unwrap(), n);
    }

    #[test]
    fn constraints_are_enforced() {
        use PixelToken::*;
        // Empty, underflow, leftovers, input beyond reference_count.
        assert!(PixelTransform::parse(&[0, 0], 1).is_err());
        assert!(PixelTransform::new(0, vec![Sum], 1).is_err());
        assert!(PixelTransform::new(0, vec![Input(1), Input(1)], 1).is_err());
        assert!(PixelTransform::new(0, vec![Input(2)], 1).is_err());
        assert!(PixelTransform::new(0, vec![Constant(300)], 0).is_err());
        // Unrecognised version / reserved token: ignored (Unsupported).
        assert!(matches!(
            PixelTransform::parse(&[0x40, 1, 1], 1),
            Err(HeifError::Unsupported(_))
        ));
        assert!(matches!(
            PixelTransform::parse(&[0, 1, 40], 1),
            Err(HeifError::Unsupported(_))
        ));
        // Truncated constant.
        assert!(matches!(
            PixelTransform::parse(&[2, 1, 0, 0, 0], 0),
            Err(HeifError::InvalidData(_))
        ));
    }
}
