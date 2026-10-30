use crate::error::{SqlError, SqlResult, SqlState};
use std::cmp::Ordering;
use std::fmt;

const BASE: u64 = 1_000_000_000;
const LIMB_DIGITS: usize = 9;
pub(crate) const MAX_INTEGRAL_DIGITS: usize = 1000;
pub(crate) const MAX_SCALE: usize = 1000;
pub(crate) const MAX_PRECISION: i64 = 1000;
const MAX_LIMBS: usize = (MAX_INTEGRAL_DIGITS + MAX_SCALE) / LIMB_DIGITS + 2;
const MIN_SIGNIFICANT_DIGITS: i64 = 16;
const KEY_NEGATIVE: u8 = 0x01;
const KEY_ZERO: u8 = 0x02;
const KEY_POSITIVE: u8 = 0x03;
const KEY_NAN: u8 = 0x04;
const ROW_NEGATIVE: u8 = 0x01;
const ROW_NAN: u8 = 0x02;

/// An exact decimal number of the `numeric` type: up to 1000 digits before the decimal point
/// and 1000 after it, or `NaN`. It keeps the number of digits after the point it was given
/// (its scale), which its text shows, while `1.5` and `1.50` compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Numeric {
    nan: bool,
    negative: bool,
    scale: u16,
    limbs: Box<[u32]>,
}

pub(crate) fn overflow() -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        "value overflows numeric format",
    )
}

fn trim(mut limbs: Vec<u32>) -> Vec<u32> {
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
    limbs
}

fn compare_magnitudes(left: &[u32], right: &[u32]) -> Ordering {
    left.len()
        .cmp(&right.len())
        .then_with(|| left.iter().rev().cmp(right.iter().rev()))
}

fn add_magnitudes(left: &[u32], right: &[u32]) -> Vec<u32> {
    let (long, short) = if left.len() >= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    let mut out = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u64;
    for (at, &limb) in long.iter().enumerate() {
        let sum = u64::from(limb) + u64::from(short.get(at).copied().unwrap_or(0)) + carry;
        out.push((sum % BASE) as u32);
        carry = sum / BASE;
    }
    if carry > 0 {
        out.push(carry as u32);
    }
    out
}

fn subtract_magnitudes(larger: &[u32], smaller: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(larger.len());
    let mut borrow = 0i64;
    for (at, &limb) in larger.iter().enumerate() {
        let mut difference =
            i64::from(limb) - i64::from(smaller.get(at).copied().unwrap_or(0)) - borrow;
        borrow = 0;
        if difference < 0 {
            difference += BASE as i64;
            borrow = 1;
        }
        out.push(difference as u32);
    }
    trim(out)
}

fn multiply_magnitudes(left: &[u32], right: &[u32]) -> Vec<u32> {
    if left.is_empty() || right.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u64; left.len() + right.len()];
    for (i, &x) in left.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in right.iter().enumerate() {
            let slot = &mut out[i + j];
            let total = *slot + u64::from(x) * u64::from(y) + carry;
            *slot = total % BASE;
            carry = total / BASE;
        }
        let mut at = i + right.len();
        while carry > 0 && at < out.len() {
            let total = out[at] + carry;
            out[at] = total % BASE;
            carry = total / BASE;
            at += 1;
        }
    }
    trim(out.into_iter().map(|limb| limb as u32).collect())
}

fn multiply_small(magnitude: &[u32], factor: u64) -> Vec<u32> {
    let mut out = Vec::with_capacity(magnitude.len() + 1);
    let mut carry = 0u64;
    for &limb in magnitude {
        let total = u64::from(limb) * factor + carry;
        out.push((total % BASE) as u32);
        carry = total / BASE;
    }
    if carry > 0 {
        out.push(carry as u32);
    }
    trim(out)
}

fn divide_small(magnitude: &[u32], divisor: u64) -> (Vec<u32>, u64) {
    let mut out = vec![0u32; magnitude.len()];
    let mut remainder = 0u64;
    for at in (0..magnitude.len()).rev() {
        let current = remainder * BASE + u64::from(magnitude[at]);
        out[at] = (current / divisor) as u32;
        remainder = current % divisor;
    }
    (trim(out), remainder)
}

fn divide_magnitudes(dividend: &[u32], divisor: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if compare_magnitudes(dividend, divisor) == Ordering::Less {
        return (Vec::new(), dividend.to_vec());
    }
    let n = divisor.len();
    if n == 1 {
        let (quotient, remainder) = divide_small(dividend, u64::from(divisor[0]));
        return (quotient, trim(vec![remainder as u32]));
    }
    let factor = BASE / (u64::from(divisor[n - 1]) + 1);
    let mut u = multiply_small(dividend, factor);
    u.resize(dividend.len() + 1, 0);
    let mut v = multiply_small(divisor, factor);
    v.resize(n, 0);
    let m = dividend.len() - n;
    let mut quotient = vec![0u32; m + 1];
    let top = u64::from(v[n - 1]);
    let next = u64::from(v[n - 2]);
    for j in (0..=m).rev() {
        let numerator = u64::from(u[j + n]) * BASE + u64::from(u[j + n - 1]);
        let mut estimate = numerator / top;
        let mut rest = numerator % top;
        while estimate >= BASE || estimate * next > rest * BASE + u64::from(u[j + n - 2]) {
            estimate -= 1;
            rest += top;
            if rest >= BASE {
                break;
            }
        }
        let mut carry = 0u64;
        let mut borrow = 0i64;
        for at in 0..n {
            let product = estimate * u64::from(v[at]) + carry;
            carry = product / BASE;
            let mut difference = i64::from(u[at + j]) - (product % BASE) as i64 - borrow;
            borrow = 0;
            if difference < 0 {
                difference += BASE as i64;
                borrow = 1;
            }
            u[at + j] = difference as u32;
        }
        let last = i64::from(u[j + n]) - carry as i64 - borrow;
        if last < 0 {
            u[j + n] = (last + BASE as i64) as u32;
            estimate -= 1;
            let mut carry = 0u64;
            for at in 0..n {
                let sum = u64::from(u[at + j]) + u64::from(v[at]) + carry;
                u[at + j] = (sum % BASE) as u32;
                carry = sum / BASE;
            }
            u[j + n] = ((u64::from(u[j + n]) + carry) % BASE) as u32;
        } else {
            u[j + n] = last as u32;
        }
        quotient[j] = estimate as u32;
    }
    u.truncate(n);
    let (remainder, _) = divide_small(&trim(u), factor);
    (trim(quotient), remainder)
}

fn power_of_ten(exponent: usize) -> Vec<u32> {
    let mut out = vec![0u32; exponent / LIMB_DIGITS];
    out.push(10u32.pow((exponent % LIMB_DIGITS) as u32));
    out
}

fn shift_left(magnitude: &[u32], digits: usize) -> Vec<u32> {
    if magnitude.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; digits / LIMB_DIGITS];
    out.extend_from_slice(magnitude);
    multiply_small(&out, u64::from(10u32.pow((digits % LIMB_DIGITS) as u32)))
}

fn round_off(magnitude: &[u32], digits: usize) -> Vec<u32> {
    if digits == 0 || magnitude.is_empty() {
        return magnitude.to_vec();
    }
    let (kept, _) = divide_magnitudes(magnitude, &power_of_ten(digits - 1));
    let (quotient, last) = divide_small(&kept, 10);
    if last >= 5 {
        add_magnitudes(&quotient, &[1])
    } else {
        quotient
    }
}

fn cut_off(magnitude: &[u32], digits: usize) -> (Vec<u32>, bool) {
    if digits == 0 || magnitude.is_empty() {
        return (magnitude.to_vec(), false);
    }
    let (quotient, remainder) = divide_magnitudes(magnitude, &power_of_ten(digits));
    (quotient, !remainder.is_empty())
}

fn digit_count(magnitude: &[u32]) -> usize {
    match magnitude.last() {
        None => 0,
        Some(&top) => (magnitude.len() - 1) * LIMB_DIGITS + top.to_string().len(),
    }
}

fn magnitude_digits(magnitude: &[u32]) -> String {
    let Some(top) = magnitude.last() else {
        return "0".into();
    };
    let mut out = top.to_string();
    for limb in magnitude.iter().rev().skip(1) {
        out.push_str(&format!("{limb:09}"));
    }
    out
}

fn magnitude_from_digits(digits: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(digits.len() / LIMB_DIGITS + 1);
    let mut end = digits.len();
    while end > 0 {
        let start = end.saturating_sub(LIMB_DIGITS);
        let limb = digits[start..end]
            .iter()
            .fold(0u32, |total, digit| total * 10 + u32::from(digit - b'0'));
        out.push(limb);
        end = start;
    }
    trim(out)
}

fn magnitude_of(value: u128) -> Vec<u32> {
    let mut out = Vec::new();
    let mut rest = value;
    while rest > 0 {
        out.push((rest % u128::from(BASE)) as u32);
        rest /= u128::from(BASE);
    }
    out
}

fn small_value(magnitude: &[u32]) -> Option<u128> {
    if magnitude.len() > 4 {
        return None;
    }
    magnitude.iter().rev().try_fold(0u128, |total, &limb| {
        total
            .checked_mul(u128::from(BASE))?
            .checked_add(u128::from(limb))
    })
}

impl Numeric {
    pub(crate) fn nan() -> Self {
        Self {
            nan: true,
            negative: false,
            scale: 0,
            limbs: Box::new([]),
        }
    }

    fn zero(scale: usize) -> Self {
        Self {
            nan: false,
            negative: false,
            scale: scale.min(MAX_SCALE) as u16,
            limbs: Box::new([]),
        }
    }

    fn finite(negative: bool, limbs: Vec<u32>, scale: usize) -> SqlResult<Self> {
        let limbs = trim(limbs);
        let (limbs, scale) = if scale > MAX_SCALE {
            (round_off(&limbs, scale - MAX_SCALE), MAX_SCALE)
        } else {
            (limbs, scale)
        };
        if digit_count(&limbs) > scale + MAX_INTEGRAL_DIGITS {
            return Err(overflow());
        }
        Ok(Self {
            nan: false,
            negative: negative && !limbs.is_empty(),
            scale: scale as u16,
            limbs: limbs.into_boxed_slice(),
        })
    }

    /// Whether the value is `NaN`.
    pub fn is_nan(&self) -> bool {
        self.nan
    }

    /// The number of digits after the decimal point the value shows.
    pub fn scale(&self) -> u16 {
        self.scale
    }

    pub(crate) fn is_zero(&self) -> bool {
        !self.nan && self.limbs.is_empty()
    }

    pub(crate) fn from_i128(value: i128) -> Self {
        Self {
            nan: false,
            negative: value < 0,
            scale: 0,
            limbs: magnitude_of(value.unsigned_abs()).into_boxed_slice(),
        }
    }

    pub(crate) fn from_scaled(units: i128, scale: u16) -> Self {
        Self {
            nan: false,
            negative: units < 0,
            scale,
            limbs: magnitude_of(units.unsigned_abs()).into_boxed_slice(),
        }
    }

    pub(crate) fn from_i64(value: i64) -> Self {
        Self::from_i128(i128::from(value))
    }

    /// Reads `text` as the `numeric` type's input does: optional spaces and sign, digits with
    /// an optional decimal point and exponent, or `NaN`.
    pub fn parse(text: &str) -> SqlResult<Self> {
        let invalid = || {
            SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("invalid input syntax for type numeric: \"{text}\""),
            )
        };
        let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.eq_ignore_ascii_case("nan") {
            return Ok(Self::nan());
        }
        let (negative, unsigned) = match trimmed.as_bytes().first() {
            Some(b'-') => (true, &trimmed[1..]),
            Some(b'+') => (false, &trimmed[1..]),
            _ => (false, trimmed),
        };
        let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
            Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
            None => (unsigned, None),
        };
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let all_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        if (whole.is_empty() && fraction.is_empty()) || !all_digits(whole) || !all_digits(fraction)
        {
            return Err(invalid());
        }
        let exponent: i64 = match exponent {
            None => 0,
            Some(written) => {
                let digits = written.strip_prefix(['+', '-']).unwrap_or(written);
                if digits.is_empty() || !all_digits(digits) {
                    return Err(invalid());
                }
                let significant = digits.trim_start_matches('0');
                if significant.len() > 6 {
                    return Err(overflow());
                }
                let value: i64 = significant.parse().unwrap_or(0);
                if written.starts_with('-') {
                    -value
                } else {
                    value
                }
            }
        };
        let mut digits: Vec<u8> = whole.bytes().chain(fraction.bytes()).collect();
        let first_significant = digits
            .iter()
            .position(|&digit| digit != b'0')
            .unwrap_or(digits.len());
        digits.drain(..first_significant);
        let scale = fraction.len() as i64 - exponent;
        if digits.is_empty() {
            return Ok(Self::zero(scale.clamp(0, MAX_SCALE as i64) as usize));
        }
        let integral = digits.len() as i64 - scale;
        if integral > MAX_INTEGRAL_DIGITS as i64 || scale > MAX_SCALE as i64 {
            return Err(overflow());
        }
        let scale = if scale < 0 {
            digits.resize(digits.len() + scale.unsigned_abs() as usize, b'0');
            0
        } else {
            scale as usize
        };
        Self::finite(negative, magnitude_from_digits(&digits), scale)
    }

    pub(crate) fn from_f64(value: f64, significant_digits: usize) -> SqlResult<Self> {
        if value.is_nan() {
            return Ok(Self::nan());
        }
        if value.is_infinite() {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot convert infinity to numeric",
            ));
        }
        let written = format!("{:.*e}", significant_digits.saturating_sub(1), value);
        let (mantissa, exponent) = written.split_once('e').unwrap_or((&written, "0"));
        let mantissa = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            mantissa
        };
        Self::parse(&format!("{mantissa}e{exponent}"))
    }

    pub(crate) fn to_f64(&self) -> f64 {
        if self.nan {
            return f64::NAN;
        }
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    pub(crate) fn to_i128_rounded(&self) -> Option<i128> {
        if self.nan {
            return None;
        }
        let rounded = round_off(&self.limbs, usize::from(self.scale));
        let magnitude = i128::try_from(small_value(&rounded)?).ok()?;
        Some(if self.negative { -magnitude } else { magnitude })
    }

    pub(crate) fn integral_value(&self) -> Option<i128> {
        if self.nan {
            return None;
        }
        let (whole, rest) = cut_off(&self.limbs, usize::from(self.scale));
        if rest {
            return None;
        }
        let magnitude = i128::try_from(small_value(&whole)?).ok()?;
        Some(if self.negative { -magnitude } else { magnitude })
    }

    fn aligned(&self, scale: usize) -> Vec<u32> {
        shift_left(&self.limbs, scale - usize::from(self.scale))
    }

    pub(crate) fn negated(&self) -> Self {
        let mut out = self.clone();
        out.negative = !self.negative && !self.limbs.is_empty() && !self.nan;
        out
    }

    pub(crate) fn absolute(&self) -> Self {
        let mut out = self.clone();
        out.negative = false;
        out
    }

    pub(crate) fn add(&self, other: &Self) -> SqlResult<Self> {
        if self.nan || other.nan {
            return Ok(Self::nan());
        }
        let scale = usize::from(self.scale.max(other.scale));
        let left = self.aligned(scale);
        let right = other.aligned(scale);
        if self.negative == other.negative {
            return Self::finite(self.negative, add_magnitudes(&left, &right), scale);
        }
        match compare_magnitudes(&left, &right) {
            Ordering::Greater => {
                Self::finite(self.negative, subtract_magnitudes(&left, &right), scale)
            }
            Ordering::Less => {
                Self::finite(other.negative, subtract_magnitudes(&right, &left), scale)
            }
            Ordering::Equal => Ok(Self::zero(scale)),
        }
    }

    pub(crate) fn subtract(&self, other: &Self) -> SqlResult<Self> {
        self.add(&other.negated())
    }

    pub(crate) fn multiply(&self, other: &Self) -> SqlResult<Self> {
        if self.nan || other.nan {
            return Ok(Self::nan());
        }
        let scale = usize::from(self.scale) + usize::from(other.scale);
        if self.is_zero() || other.is_zero() {
            return Ok(Self::zero(scale));
        }
        let integral = |value: &Self| digit_count(&value.limbs) as i64 - i64::from(value.scale);
        if integral(self) + integral(other) - 1 > MAX_INTEGRAL_DIGITS as i64 {
            return Err(overflow());
        }
        Self::finite(
            self.negative != other.negative,
            multiply_magnitudes(&self.limbs, &other.limbs),
            scale,
        )
    }

    fn decimal_exponent(&self) -> Option<i64> {
        (!self.limbs.is_empty())
            .then(|| digit_count(&self.limbs) as i64 - 1 - i64::from(self.scale))
    }

    fn base_10000_weight(&self) -> (i64, u32) {
        let Some(exponent) = self.decimal_exponent() else {
            return (0, 0);
        };
        let weight = exponent.div_euclid(4);
        let leading = (exponent - 4 * weight + 1) as usize;
        let digits = magnitude_digits(&self.limbs);
        let first = digits
            .bytes()
            .take(leading)
            .fold(0u32, |total, digit| total * 10 + u32::from(digit - b'0'));
        (weight, first)
    }

    fn division_scale(&self, other: &Self) -> usize {
        let (left_weight, left_first) = self.base_10000_weight();
        let (right_weight, right_first) = other.base_10000_weight();
        let mut weight = left_weight - right_weight;
        if left_first <= right_first {
            weight -= 1;
        }
        let scale = (MIN_SIGNIFICANT_DIGITS - weight * 4)
            .max(i64::from(self.scale))
            .max(i64::from(other.scale))
            .max(0);
        scale.min(MAX_SCALE as i64) as usize
    }

    pub(crate) fn divide(&self, other: &Self) -> SqlResult<Self> {
        if self.nan || other.nan {
            return Ok(Self::nan());
        }
        if other.is_zero() {
            return Err(SqlError::new(
                SqlState::DIVISION_BY_ZERO,
                "division by zero",
            ));
        }
        let scale = self.division_scale(other);
        self.divide_to_scale(other, scale)
    }

    pub(crate) fn divide_truncated(&self, other: &Self, scale: usize) -> SqlResult<Self> {
        if self.nan || other.nan {
            return Ok(Self::nan());
        }
        if other.is_zero() {
            return Err(SqlError::new(
                SqlState::DIVISION_BY_ZERO,
                "division by zero",
            ));
        }
        let scale = scale.min(MAX_SCALE);
        if self.is_zero() {
            return Ok(Self::zero(scale));
        }
        let estimate = self.decimal_exponent().unwrap_or(0) - other.decimal_exponent().unwrap_or(0);
        if estimate > MAX_INTEGRAL_DIGITS as i64 {
            return Err(overflow());
        }
        let shift = i64::from(other.scale) - i64::from(self.scale) + scale as i64;
        let (dividend, divisor) = if shift >= 0 {
            (
                shift_left(&self.limbs, shift as usize),
                other.limbs.to_vec(),
            )
        } else {
            (
                self.limbs.to_vec(),
                shift_left(&other.limbs, shift.unsigned_abs() as usize),
            )
        };
        let (quotient, _) = divide_magnitudes(&dividend, &divisor);
        Self::finite(self.negative != other.negative, quotient, scale)
    }

    fn divide_to_scale(&self, other: &Self, scale: usize) -> SqlResult<Self> {
        if self.is_zero() {
            return Ok(Self::zero(scale));
        }
        let estimate = self.decimal_exponent().unwrap_or(0) - other.decimal_exponent().unwrap_or(0);
        if estimate > MAX_INTEGRAL_DIGITS as i64 {
            return Err(overflow());
        }
        let shift = i64::from(other.scale) - i64::from(self.scale) + scale as i64 + 1;
        let (dividend, divisor) = if shift >= 0 {
            (
                shift_left(&self.limbs, shift as usize),
                other.limbs.to_vec(),
            )
        } else {
            (
                self.limbs.to_vec(),
                shift_left(&other.limbs, shift.unsigned_abs() as usize),
            )
        };
        let (quotient, _) = divide_magnitudes(&dividend, &divisor);
        let rounded = round_off(&quotient, 1);
        Self::finite(self.negative != other.negative, rounded, scale)
    }

    pub(crate) fn remainder(&self, other: &Self) -> SqlResult<Self> {
        if self.nan || other.nan {
            return Ok(Self::nan());
        }
        if other.is_zero() {
            return Err(SqlError::new(
                SqlState::DIVISION_BY_ZERO,
                "division by zero",
            ));
        }
        let scale = usize::from(self.scale.max(other.scale));
        let (_, remainder) = divide_magnitudes(&self.aligned(scale), &other.aligned(scale));
        Self::finite(self.negative, remainder, scale)
    }

    pub(crate) fn compare(&self, other: &Self) -> Ordering {
        match (self.nan, other.nan) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        let sign = |value: &Self| {
            if value.limbs.is_empty() {
                0
            } else if value.negative {
                -1
            } else {
                1
            }
        };
        let by_sign = sign(self).cmp(&sign(other));
        if by_sign != Ordering::Equal || sign(self) == 0 {
            return by_sign;
        }
        let ordering = match (self.decimal_exponent(), other.decimal_exponent()) {
            (Some(left), Some(right)) if left != right => left.cmp(&right),
            _ => {
                let scale = usize::from(self.scale.max(other.scale));
                compare_magnitudes(&self.aligned(scale), &other.aligned(scale))
            }
        };
        if self.negative {
            ordering.reverse()
        } else {
            ordering
        }
    }

    pub(crate) fn rounded(&self, places: i64) -> SqlResult<Self> {
        self.rescaled(places, true)
    }

    pub(crate) fn truncated(&self, places: i64) -> SqlResult<Self> {
        self.rescaled(places, false)
    }

    fn rescaled(&self, places: i64, round: bool) -> SqlResult<Self> {
        if self.nan {
            return Ok(Self::nan());
        }
        let places = places.clamp(-(MAX_INTEGRAL_DIGITS as i64) - 1, MAX_SCALE as i64);
        let current = i64::from(self.scale);
        if places >= current {
            let scale = places as usize;
            return Self::finite(self.negative, self.aligned(scale), scale);
        }
        let dropped = (current - places) as usize;
        let kept = if round {
            round_off(&self.limbs, dropped)
        } else {
            cut_off(&self.limbs, dropped).0
        };
        if places >= 0 {
            Self::finite(self.negative, kept, places as usize)
        } else {
            Self::finite(
                self.negative,
                shift_left(&kept, places.unsigned_abs() as usize),
                0,
            )
        }
    }

    pub(crate) fn ceiling(&self) -> SqlResult<Self> {
        self.to_integer_toward(false)
    }

    pub(crate) fn floor(&self) -> SqlResult<Self> {
        self.to_integer_toward(true)
    }

    fn to_integer_toward(&self, down: bool) -> SqlResult<Self> {
        if self.nan {
            return Ok(Self::nan());
        }
        let (whole, cut) = cut_off(&self.limbs, usize::from(self.scale));
        let away = cut && (self.negative == down);
        let whole = if away {
            add_magnitudes(&whole, &[1])
        } else {
            whole
        };
        Self::finite(self.negative, whole, 0)
    }

    pub(crate) fn with_modifier(&self, precision: i64, scale: i64) -> SqlResult<Self> {
        if self.nan {
            return Ok(Self::nan());
        }
        let rounded = self.rounded(scale)?;
        if rounded.is_zero() {
            return Ok(rounded);
        }
        let integral = digit_count(&rounded.limbs) as i64 - i64::from(rounded.scale);
        let allowed = precision - scale;
        if integral > allowed {
            let bound = if allowed == 0 {
                "1".to_owned()
            } else {
                format!("10^{allowed}")
            };
            return Err(SqlError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                "numeric field overflow",
            )
            .with_detail(format!(
                "A field with precision {precision}, scale {scale} must round to an absolute value less than {bound}."
            )));
        }
        Ok(rounded)
    }

    pub(crate) fn power(&self, exponent: &Self) -> SqlResult<Self> {
        let invalid = |message: &str| SqlError::new(SqlState::INVALID_ARGUMENT_FOR_POWER, message);
        if exponent.is_zero() && !self.nan {
            return Self::from_i64(1).rescaled(MIN_SIGNIFICANT_DIGITS, true);
        }
        if self.nan || exponent.nan {
            let one = Self::from_i64(1);
            if (self.nan && exponent.is_zero())
                || (!self.nan && self.compare(&one) == Ordering::Equal)
            {
                return Ok(one);
            }
            return Ok(Self::nan());
        }
        let integral = exponent.is_integral();
        let odd = integral && exponent.is_odd();
        if self.is_zero() {
            if exponent.negative {
                return Err(invalid("zero raised to a negative power is undefined"));
            }
            return Ok(Self::zero(MIN_SIGNIFICANT_DIGITS as usize));
        }
        if self.negative && !integral {
            return Err(invalid(
                "a negative number raised to a non-integer power yields a complex result",
            ));
        }
        let floor_scale = i64::from(self.scale).max(i64::from(exponent.scale));
        let one = Self::from_i64(1);
        if self.absolute().compare(&one) == Ordering::Equal {
            let magnitude = one.rescaled(MIN_SIGNIFICANT_DIGITS.max(floor_scale), true)?;
            return Ok(if self.negative && odd {
                magnitude.negated()
            } else {
                magnitude
            });
        }
        let estimate = self.log10_estimate() * exponent.to_f64();
        if estimate.is_nan() || estimate > MAX_INTEGRAL_DIGITS as f64 + 1.0 {
            return Err(overflow());
        }
        let bounded = estimate.max(-4.0 * MAX_SCALE as f64);
        let scale = (MIN_SIGNIFICANT_DIGITS - bounded.floor() as i64)
            .max(floor_scale)
            .clamp(0, MAX_SCALE as i64) as usize;
        if bounded < -(scale as f64) - 2.0 {
            return Ok(Self::zero(scale));
        }
        if integral
            && let Some(count) = exponent.integral_value()
            && let Some(exact) = self.exact_power(count, scale)?
        {
            return Ok(exact);
        }
        let approximate = self.to_f64().abs().powf(exponent.to_f64());
        let magnitude = if approximate.is_finite() && approximate != 0.0 {
            Self::from_f64(approximate, 15)?
        } else {
            let whole = estimate.floor();
            let mantissa = 10f64.powf(estimate - whole);
            Self::parse(&format!("{mantissa:.14}e{}", whole as i64))?
        };
        let signed = if self.negative && odd {
            magnitude.negated()
        } else {
            magnitude
        };
        signed.rounded(scale as i64)
    }

    fn is_integral(&self) -> bool {
        !self.nan && !cut_off(&self.limbs, usize::from(self.scale)).1
    }

    fn is_odd(&self) -> bool {
        let (whole, _) = cut_off(&self.limbs, usize::from(self.scale));
        whole.first().is_some_and(|limb| limb % 2 == 1)
    }

    fn log10_estimate(&self) -> f64 {
        let digits = magnitude_digits(&self.limbs);
        let leading: f64 = digits
            .get(..digits.len().min(15))
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let leading_digits = digits.len().min(15) as f64;
        leading.log10() - (leading_digits - 1.0) + self.decimal_exponent().unwrap_or(0) as f64
    }

    fn exact_power(&self, count: i128, scale: usize) -> SqlResult<Option<Self>> {
        let steps = count.unsigned_abs();
        let exact_scale = u128::from(self.scale).saturating_mul(steps);
        let exact_digits = (digit_count(&self.limbs) as u128).saturating_mul(steps);
        if exact_scale > (2 * MAX_SCALE) as u128
            || exact_digits > (2 * (MAX_INTEGRAL_DIGITS + MAX_SCALE)) as u128
        {
            return Ok(None);
        }
        let mut result = vec![1u32];
        let mut base = self.limbs.to_vec();
        let mut rest = steps;
        while rest > 0 {
            if rest & 1 == 1 {
                result = multiply_magnitudes(&result, &base);
            }
            rest >>= 1;
            if rest > 0 {
                base = multiply_magnitudes(&base, &base);
            }
        }
        let negative = self.negative && steps % 2 == 1;
        let exact_scale = exact_scale as usize;
        if count >= 0 {
            let value = Self {
                nan: false,
                negative: negative && !result.is_empty(),
                scale: 0,
                limbs: result.into_boxed_slice(),
            };
            let rounded = if exact_scale > scale {
                round_off(&value.limbs, exact_scale - scale)
            } else {
                shift_left(&value.limbs, scale - exact_scale)
            };
            return Self::finite(negative, rounded, scale).map(Some);
        }
        let denominator = Self {
            nan: false,
            negative,
            scale: exact_scale as u16,
            limbs: result.into_boxed_slice(),
        };
        Self::from_i64(1)
            .divide_to_scale(&denominator, scale)
            .map(Some)
    }

    pub(crate) fn encode_key(&self, out: &mut Vec<u8>) {
        if self.nan {
            out.push(KEY_NAN);
            return;
        }
        let Some(exponent) = self.decimal_exponent() else {
            out.push(KEY_ZERO);
            return;
        };
        let digits = magnitude_digits(&self.limbs);
        let digits = digits.trim_end_matches('0');
        let exponent = (exponent + 0x8000) as u16;
        if self.negative {
            out.push(KEY_NEGATIVE);
            out.extend_from_slice(&(!exponent).to_be_bytes());
            out.extend(digits.bytes().map(|digit| 0xFE - (digit - b'0')));
            out.push(0xFF);
        } else {
            out.push(KEY_POSITIVE);
            out.extend_from_slice(&exponent.to_be_bytes());
            out.extend(digits.bytes().map(|digit| digit - b'0' + 1));
            out.push(0x00);
        }
    }

    pub(crate) fn key_length(key: &[u8]) -> Option<usize> {
        match *key.first()? {
            KEY_NAN | KEY_ZERO => Some(1),
            class @ (KEY_NEGATIVE | KEY_POSITIVE) => {
                let terminator = if class == KEY_NEGATIVE { 0xFF } else { 0x00 };
                let digits = key.get(3..)?;
                let end = digits
                    .iter()
                    .take(MAX_INTEGRAL_DIGITS + MAX_SCALE + 1)
                    .position(|&byte| byte == terminator)?;
                Some(3 + end + 1)
            }
            _ => None,
        }
    }

    pub(crate) fn decode_key(key: &[u8]) -> Option<(Self, usize)> {
        let length = Self::key_length(key)?;
        let class = *key.first()?;
        match class {
            KEY_NAN => return Some((Self::nan(), 1)),
            KEY_ZERO => return Some((Self::zero(0), 1)),
            _ => {}
        }
        let negative = class == KEY_NEGATIVE;
        let mut exponent = u16::from_be_bytes([*key.get(1)?, *key.get(2)?]);
        if negative {
            exponent = !exponent;
        }
        let exponent = i64::from(exponent) - 0x8000;
        let encoded = key.get(3..length - 1)?;
        let mut digits = Vec::with_capacity(encoded.len());
        for &byte in encoded {
            let digit = if negative {
                0xFEu8.checked_sub(byte)?
            } else {
                byte.checked_sub(1)?
            };
            if digit > 9 {
                return None;
            }
            digits.push(b'0' + digit);
        }
        if digits.first() == Some(&b'0') || digits.last() == Some(&b'0') || digits.is_empty() {
            return None;
        }
        let scale = digits.len() as i64 - 1 - exponent;
        if scale > MAX_SCALE as i64 || exponent >= MAX_INTEGRAL_DIGITS as i64 {
            return None;
        }
        if scale < 0 {
            digits.resize(digits.len() + scale.unsigned_abs() as usize, b'0');
        }
        let value = Self::finite(
            negative,
            magnitude_from_digits(&digits),
            scale.max(0) as usize,
        )
        .ok()?;
        Some((value, length))
    }

    pub(crate) fn encode_row(&self, out: &mut Vec<u8>) {
        let mut flags = 0;
        if self.negative {
            flags |= ROW_NEGATIVE;
        }
        if self.nan {
            flags |= ROW_NAN;
        }
        out.push(flags);
        out.extend_from_slice(&self.scale.to_le_bytes());
        out.push(self.limbs.len() as u8);
        for limb in &self.limbs {
            out.extend_from_slice(&limb.to_le_bytes());
        }
    }

    pub(crate) fn decode_row(bytes: &[u8]) -> Option<(Self, usize)> {
        let flags = *bytes.first()?;
        let scale = u16::from_le_bytes([*bytes.get(1)?, *bytes.get(2)?]);
        let count = usize::from(*bytes.get(3)?);
        if flags & !(ROW_NEGATIVE | ROW_NAN) != 0
            || usize::from(scale) > MAX_SCALE
            || count > MAX_LIMBS
        {
            return None;
        }
        let end = 4 + count * 4;
        let body = bytes.get(4..end)?;
        let limbs: Vec<u32> = body
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| u32::from_le_bytes(*chunk))
            .collect();
        if limbs.iter().any(|&limb| u64::from(limb) >= BASE) || limbs.last() == Some(&0) {
            return None;
        }
        if flags & ROW_NAN != 0 {
            return (flags == ROW_NAN && scale == 0 && limbs.is_empty())
                .then(|| (Self::nan(), end));
        }
        let negative = flags & ROW_NEGATIVE != 0;
        if negative && limbs.is_empty() {
            return None;
        }
        if digit_count(&limbs) > usize::from(scale) + MAX_INTEGRAL_DIGITS {
            return None;
        }
        Some((
            Self {
                nan: false,
                negative,
                scale,
                limbs: limbs.into_boxed_slice(),
            },
            end,
        ))
    }

    pub(crate) fn exact_double(&self) -> Option<f64> {
        if self.nan {
            return None;
        }
        let digits = magnitude_digits(&self.limbs);
        let significant = digits.trim_end_matches('0');
        let trimmed = digits.len() - significant.len();
        let scale = usize::from(self.scale).saturating_sub(trimmed);
        let units: u128 = if scale == 0 {
            small_value(&self.limbs)?
        } else {
            significant.parse().ok()?
        };
        let (quotient, twos) = if scale == 0 {
            (units, 0)
        } else {
            let five_power = 5u128.checked_pow(u32::try_from(scale).ok()?)?;
            if !units.is_multiple_of(five_power) {
                return None;
            }
            (units / five_power, scale)
        };
        let odd = quotient >> quotient.trailing_zeros().min(127);
        if odd >= 1 << 53 {
            return None;
        }
        let value = self.to_f64();
        let exponent = i32::try_from(twos).ok()?;
        let rebuilt = (quotient as f64) * 2f64.powi(-exponent);
        (value.is_finite() && rebuilt == value.abs() && value.abs() >= f64::MIN_POSITIVE)
            .then_some(value)
    }

    pub(crate) fn equality_text(&self) -> String {
        if self.nan {
            return "NaN".into();
        }
        let Some(exponent) = self.decimal_exponent() else {
            return "0".into();
        };
        let digits = magnitude_digits(&self.limbs);
        let sign = if self.negative { "-" } else { "" };
        format!("{sign}{}e{exponent}", digits.trim_end_matches('0'))
    }
}

impl fmt::Display for Numeric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.nan {
            return f.write_str("NaN");
        }
        if self.negative {
            f.write_str("-")?;
        }
        let digits = magnitude_digits(&self.limbs);
        let scale = usize::from(self.scale);
        if scale == 0 {
            return f.write_str(&digits);
        }
        if digits.len() > scale {
            let (whole, fraction) = digits.split_at(digits.len() - scale);
            write!(f, "{whole}.{fraction}")
        } else {
            f.write_str("0.")?;
            for _ in digits.len()..scale {
                f.write_str("0")?;
            }
            f.write_str(&digits)
        }
    }
}

pub(crate) fn check_modifier(precision: i64, scale: i64) -> SqlResult<()> {
    if !(1..=MAX_PRECISION).contains(&precision) {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("NUMERIC precision {precision} must be between 1 and {MAX_PRECISION}"),
        ));
    }
    if !(-MAX_PRECISION..=MAX_PRECISION).contains(&scale) {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("NUMERIC scale {scale} must be between -{MAX_PRECISION} and {MAX_PRECISION}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_INTEGRAL_DIGITS, Numeric};
    use std::cmp::Ordering;

    fn n(text: &str) -> Numeric {
        Numeric::parse(text).unwrap()
    }

    fn shown(result: crate::error::SqlResult<Numeric>) -> String {
        match result {
            Ok(value) => value.to_string(),
            Err(error) => format!("error: {}", error.message()),
        }
    }

    #[test]
    fn input_keeps_its_scale_and_prints_like_postgresql() {
        let cases = [
            ("1.50", "1.50"),
            (" -0.0 ", "0.0"),
            ("+12", "12"),
            (".5", "0.5"),
            ("5.", "5"),
            ("1e3", "1000"),
            ("1.5e-3", "0.0015"),
            ("0001.2300", "1.2300"),
            ("nan", "NaN"),
            ("-0", "0"),
            (
                "123456789012345678901234567890.123456789",
                "123456789012345678901234567890.123456789",
            ),
        ];
        for (input, output) in cases {
            assert_eq!(n(input).to_string(), output, "{input}");
        }
        for bad in [
            "", "abc", "1.2.3", "1e", "--1", "1 2", ".", "e5", "Infinity",
        ] {
            assert_eq!(
                Numeric::parse(bad).unwrap_err().state().code(),
                "22P02",
                "{bad}"
            );
        }
        assert_eq!(
            Numeric::parse(&format!("1{}", "0".repeat(MAX_INTEGRAL_DIGITS)))
                .unwrap_err()
                .message(),
            "value overflows numeric format"
        );
        assert!(Numeric::parse(&"9".repeat(MAX_INTEGRAL_DIGITS)).is_ok());
        assert!(Numeric::parse("1e999999999999").is_err());
        assert!(Numeric::parse("1e-1001").is_err());
    }

    #[test]
    fn arithmetic_follows_postgresql_scales() {
        assert_eq!(shown(n("1.5").add(&n("2.25"))), "3.75");
        assert_eq!(shown(n("1.50").subtract(&n("1.5"))), "0.00");
        assert_eq!(shown(n("1.5").multiply(&n("-2.25"))), "-3.375");
        assert_eq!(shown(n("1").divide(&n("3"))), "0.33333333333333333333");
        assert_eq!(shown(n("10").divide(&n("4"))), "2.5000000000000000");
        assert_eq!(shown(n("2").divide(&n("3"))), "0.66666666666666666667");
        assert_eq!(shown(n("-2").divide(&n("3"))), "-0.66666666666666666667");
        assert_eq!(shown(n("7.5").remainder(&n("2"))), "1.5");
        assert_eq!(shown(n("-7.5").remainder(&n("2"))), "-1.5");
        assert_eq!(shown(n("1").divide(&n("0"))), "error: division by zero");
        assert_eq!(shown(n("NaN").add(&n("1"))), "NaN");
        assert_eq!(shown(n("2").power(&n("10"))), "1024.0000000000000");
        assert_eq!(
            shown(n("0").power(&n("-1"))),
            "error: zero raised to a negative power is undefined"
        );
        assert_eq!(
            shown(n("10").power(&n("2000"))),
            "error: value overflows numeric format"
        );
        assert_eq!(shown(n("2").power(&n("-2"))), "0.25000000000000000");
    }

    #[test]
    fn truncated_division_drops_the_digits_past_its_scale() {
        assert_eq!(shown(n("2").divide_truncated(&n("3"), 6)), "0.666666");
        assert_eq!(shown(n("-2").divide_truncated(&n("3"), 2)), "-0.66");
        assert_eq!(shown(n("10.0").divide_truncated(&n("4"), 1)), "2.5");
        assert_eq!(shown(n("0").divide_truncated(&n("7"), 3)), "0.000");
        assert_eq!(
            shown(n("1").divide_truncated(&n("0"), 6)),
            "error: division by zero"
        );
    }

    #[test]
    fn rounding_is_half_away_from_zero() {
        assert_eq!(shown(n("2.5").rounded(0)), "3");
        assert_eq!(shown(n("-2.5").rounded(0)), "-3");
        assert_eq!(shown(n("1.2345").rounded(2)), "1.23");
        assert_eq!(shown(n("1.235").rounded(2)), "1.24");
        assert_eq!(shown(n("1.5").rounded(3)), "1.500");
        assert_eq!(shown(n("1250").rounded(-2)), "1300");
        assert_eq!(shown(n("-1.99").truncated(1)), "-1.9");
        assert_eq!(shown(n("-1.1").ceiling()), "-1");
        assert_eq!(shown(n("-1.1").floor()), "-2");
        assert_eq!(shown(n("1.1").ceiling()), "2");
        assert_eq!(shown(n("2").floor()), "2");
    }

    #[test]
    fn modifiers_round_then_check_the_digits_before_the_point() {
        assert_eq!(shown(n("123.456").with_modifier(5, 2)), "123.46");
        assert_eq!(
            shown(n("1234.5").with_modifier(5, 2)),
            "error: numeric field overflow"
        );
        assert_eq!(
            shown(n("999.995").with_modifier(5, 2)),
            "error: numeric field overflow"
        );
        assert_eq!(shown(n("0.001234").with_modifier(3, 5)), "0.00123");
        assert!(n("0.01").with_modifier(3, 5).is_err());
        assert_eq!(shown(n("12345").with_modifier(2, -3)), "12000");
        assert!(n("123456").with_modifier(2, -3).is_err());
        assert_eq!(shown(n("NaN").with_modifier(3, 1)), "NaN");
    }

    #[test]
    fn comparison_ignores_trailing_zeros_and_puts_nan_last() {
        assert_eq!(n("1.5").compare(&n("1.50")), Ordering::Equal);
        assert_eq!(n("-1").compare(&n("0")), Ordering::Less);
        assert_eq!(n("-10").compare(&n("-9.99")), Ordering::Less);
        assert_eq!(n("NaN").compare(&n("1e900")), Ordering::Greater);
        assert_eq!(n("0.001").compare(&n("0.0001")), Ordering::Greater);
    }

    #[test]
    fn floats_convert_with_their_significant_digits() {
        assert_eq!(shown(Numeric::from_f64(0.1, 15)), "0.1");
        assert_eq!(shown(Numeric::from_f64(2.5, 15)), "2.5");
        assert_eq!(shown(Numeric::from_f64(1e20, 15)), "100000000000000000000");
        assert_eq!(shown(Numeric::from_f64(-0.0, 15)), "0");
        assert_eq!(shown(Numeric::from_f64(1.0 / 3.0, 15)), "0.333333333333333");
        assert!(Numeric::from_f64(f64::INFINITY, 15).is_err());
        assert_eq!(n("2.5").to_f64(), 2.5);
        assert_eq!(n("2.5").to_i128_rounded(), Some(3));
        assert_eq!(n("-2.5").to_i128_rounded(), Some(-3));
    }

    struct Seeded(u64);

    impl Seeded {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn digits(&mut self, count: usize) -> String {
            let mut out: String = (0..count)
                .map(|_| char::from(b'0' + (self.next() % 10) as u8))
                .collect();
            if out.is_empty() {
                out.push('0');
            }
            out
        }
    }

    fn reference_trim(digits: &[u8]) -> Vec<u8> {
        let start = digits.iter().position(|&d| d != 0).unwrap_or(digits.len());
        digits[start..].to_vec()
    }

    fn reference_compare(left: &[u8], right: &[u8]) -> Ordering {
        let (left, right) = (reference_trim(left), reference_trim(right));
        left.len().cmp(&right.len()).then_with(|| left.cmp(&right))
    }

    fn reference_add(left: &[u8], right: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut i, mut j, mut carry) = (left.len(), right.len(), 0);
        while i > 0 || j > 0 || carry > 0 {
            let a = if i > 0 {
                i -= 1;
                left[i]
            } else {
                0
            };
            let b = if j > 0 {
                j -= 1;
                right[j]
            } else {
                0
            };
            let sum = a + b + carry;
            out.push(sum % 10);
            carry = sum / 10;
        }
        out.reverse();
        reference_trim(&out)
    }

    fn reference_subtract(left: &[u8], right: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut i, mut j, mut borrow) = (left.len(), right.len(), 0i8);
        while i > 0 {
            i -= 1;
            let b = if j > 0 {
                j -= 1;
                right[j] as i8
            } else {
                0
            };
            let mut digit = left[i] as i8 - b - borrow;
            borrow = 0;
            if digit < 0 {
                digit += 10;
                borrow = 1;
            }
            out.push(digit as u8);
        }
        out.reverse();
        reference_trim(&out)
    }

    fn reference_multiply(left: &[u8], right: &[u8]) -> Vec<u8> {
        let mut out = vec![0u32; left.len() + right.len()];
        for (i, &a) in left.iter().rev().enumerate() {
            for (j, &b) in right.iter().rev().enumerate() {
                out[i + j] += u32::from(a) * u32::from(b);
            }
        }
        let mut carry = 0;
        for slot in &mut out {
            let total = *slot + carry;
            *slot = total % 10;
            carry = total / 10;
        }
        let digits: Vec<u8> = out.iter().rev().map(|&d| d as u8).collect();
        reference_trim(&digits)
    }

    fn reference_divide(left: &[u8], right: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut quotient = Vec::new();
        let mut rest: Vec<u8> = Vec::new();
        for &digit in left {
            rest.push(digit);
            rest = reference_trim(&rest);
            let mut count = 0;
            while reference_compare(&rest, right) != Ordering::Less {
                rest = reference_subtract(&rest, right);
                count += 1;
            }
            quotient.push(count);
        }
        (reference_trim(&quotient), rest)
    }

    fn as_digits(text: &str) -> Vec<u8> {
        reference_trim(&text.bytes().map(|b| b - b'0').collect::<Vec<_>>())
    }

    fn as_text(digits: &[u8]) -> String {
        if digits.is_empty() {
            return "0".into();
        }
        digits.iter().map(|&d| char::from(b'0' + d)).collect()
    }

    #[test]
    fn integer_arithmetic_matches_a_schoolbook_reference() {
        let mut random = Seeded(0x5DEE_CE66_D1CE_4E5B);
        for round in 0..400 {
            let left_length = (random.next() % 80) as usize + 1;
            let right_length = if round % 3 == 0 {
                (random.next() % 12) as usize + 1
            } else {
                (random.next() % 60) as usize + 1
            };
            let left_text = random.digits(left_length);
            let right_text = random.digits(right_length);
            let (left, right) = (as_digits(&left_text), as_digits(&right_text));
            let a = n(&left_text);
            let b = n(&right_text);
            assert_eq!(
                a.add(&b).unwrap().to_string(),
                as_text(&reference_add(&left, &right))
            );
            assert_eq!(
                a.multiply(&b).unwrap().to_string(),
                as_text(&reference_multiply(&left, &right))
            );
            let difference = a.subtract(&b).unwrap().to_string();
            let expected = match reference_compare(&left, &right) {
                Ordering::Less => format!("-{}", as_text(&reference_subtract(&right, &left))),
                _ => as_text(&reference_subtract(&left, &right)),
            };
            assert_eq!(difference, expected);
            if right.is_empty() {
                continue;
            }
            let (quotient, rest) = reference_divide(&left, &right);
            let truncated = a.divide(&b).unwrap().truncated(0).unwrap();
            let exact = n(&as_text(&quotient));
            let difference = truncated.subtract(&exact).unwrap();
            assert!(
                difference.is_zero() || difference.compare(&n("1")) == Ordering::Equal,
                "{left_text} / {right_text}"
            );
            assert_eq!(
                a.remainder(&b).unwrap().to_string(),
                as_text(&rest),
                "{left_text} % {right_text}"
            );
        }
    }

    #[test]
    fn small_operands_match_i128() {
        let mut random = Seeded(0x0BAD_5EED_1234_5678);
        for _ in 0..3000 {
            let a = (random.next() as i64 >> (random.next() % 40)) as i128;
            let b = (random.next() as i64 >> (random.next() % 40)) as i128;
            let left = Numeric::from_i128(a);
            let right = Numeric::from_i128(b);
            assert_eq!(left.add(&right).unwrap().to_string(), (a + b).to_string());
            assert_eq!(
                left.subtract(&right).unwrap().to_string(),
                (a - b).to_string()
            );
            assert_eq!(
                left.multiply(&right).unwrap().to_string(),
                (a * b).to_string()
            );
            assert_eq!(left.compare(&right), a.cmp(&b));
            if b != 0 {
                assert_eq!(
                    left.remainder(&right).unwrap().to_string(),
                    (a % b).to_string()
                );
                let quotient = left.divide(&right).unwrap();
                assert_eq!(
                    quotient.truncated(0).unwrap().to_string(),
                    (a / b).to_string()
                );
            }
            let mut key_a = Vec::new();
            let mut key_b = Vec::new();
            left.encode_key(&mut key_a);
            right.encode_key(&mut key_b);
            assert_eq!(key_a.cmp(&key_b), a.cmp(&b));
        }
    }

    #[test]
    fn keys_order_like_values_and_decode_back() {
        let mut random = Seeded(0xC0FF_EE00_DEAD_BEEF);
        let mut values = vec![
            n("NaN"),
            n("0"),
            n("0.000"),
            n("-0.5"),
            n("1e900"),
            n("-1e900"),
        ];
        for _ in 0..600 {
            let whole_length = (random.next() % 6) as usize;
            let whole = random.digits(whole_length);
            let fraction_length = (random.next() % 6) as usize;
            let fraction = random.digits(fraction_length);
            let sign = if random.next().is_multiple_of(2) {
                "-"
            } else {
                ""
            };
            values.push(n(&format!("{sign}{whole}.{fraction}")));
        }
        for left in &values {
            let mut left_key = Vec::new();
            left.encode_key(&mut left_key);
            let (decoded, used) = Numeric::decode_key(&left_key).unwrap();
            assert_eq!(used, left_key.len());
            assert_eq!(decoded.compare(left), Ordering::Equal);
            for right in values.iter().take(80) {
                let mut right_key = Vec::new();
                right.encode_key(&mut right_key);
                assert_eq!(
                    left_key.cmp(&right_key),
                    left.compare(right),
                    "{left} {right}"
                );
            }
        }
    }

    #[test]
    fn rows_round_trip_and_hostile_bytes_are_refused() {
        for text in [
            "0",
            "0.00",
            "-1.5",
            "NaN",
            "123456789012345678901234567890.5",
        ] {
            let value = n(text);
            let mut bytes = Vec::new();
            value.encode_row(&mut bytes);
            assert_eq!(
                Numeric::decode_row(&bytes),
                Some((value.clone(), bytes.len()))
            );
            for cut in 0..bytes.len() {
                assert!(Numeric::decode_row(&bytes[..cut]).is_none());
            }
        }
        assert!(Numeric::decode_row(&[0x04, 0, 0, 0]).is_none());
        assert!(Numeric::decode_row(&[0, 0xFF, 0xFF, 0]).is_none());
        assert!(Numeric::decode_row(&[0, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFF]).is_none());
        assert!(Numeric::decode_row(&[0, 0, 0, 1, 0, 0, 0, 0]).is_none());
        assert!(Numeric::decode_row(&[0x01, 0, 0, 0]).is_none());
        assert!(Numeric::decode_row(&[0, 0, 0, 255]).is_none());
        let mut random = Seeded(42);
        for _ in 0..20_000 {
            let length = (random.next() % 24) as usize;
            let bytes: Vec<u8> = (0..length).map(|_| random.next() as u8).collect();
            let _ = Numeric::decode_row(&bytes);
            let _ = Numeric::decode_key(&bytes);
        }
    }

    #[test]
    fn hostile_sizes_are_bounded_errors() {
        let big = n(&"9".repeat(MAX_INTEGRAL_DIGITS));
        assert!(big.multiply(&big).is_err());
        assert!(big.add(&big).is_err());
        let tiny = n(&format!("0.{}1", "0".repeat(998)));
        assert!(big.divide(&tiny).is_err());
        assert!(tiny.divide(&big).unwrap().is_zero());
        assert!(n("9.99").power(&n("100000")).is_err());
        assert!(n("0.5").power(&n("100000")).unwrap().is_zero());
        assert!(n("1.0000001").power(&n("99999")).is_ok());
        assert!(
            n("0.5")
                .power(&n("99999999999999999999999"))
                .unwrap()
                .is_zero()
        );
        assert!(n("0.5").power(&n("1e400")).unwrap().is_zero());
        assert_eq!(shown(n("1").power(&n("1e400"))), "1.0000000000000000");
        assert_eq!(shown(n("-1").power(&n("1e400"))), "1.0000000000000000");
        assert_eq!(shown(n("-1").power(&n("3"))), "-1.0000000000000000");
        assert!(n("2").power(&n("99999999999999999999999")).is_err());
        assert!(
            n("2")
                .power(&n("-99999999999999999999999"))
                .unwrap()
                .is_zero()
        );
        let large = n("10").power(&n("400.5")).unwrap();
        assert!(large.to_string().starts_with("316227766016838"), "{large}");
        assert_eq!(large.scale(), 1);
        let small = n("10").power(&n("-400.5")).unwrap();
        assert_eq!(small.scale(), 417);
        assert!(small.to_string().ends_with("31622776601683800"), "{small}");
        assert_eq!(
            shown(n("-2").power(&n("0.5"))),
            "error: a negative number raised to a non-integer power yields a complex result"
        );
        assert_eq!(shown(n("-2").power(&n("3.0"))), "-8.0000000000000000");
        let mut seed = 0x00DD_5EED_u64;
        for _ in 0..2_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let base = format!(
                "{}{}.{}",
                if seed.is_multiple_of(3) { "-" } else { "" },
                seed % 97,
                (seed >> 9) % 1000
            );
            let exponent = format!("{}e{}", (seed >> 20) % 1000, (seed >> 30) % 40);
            let negative = format!("-{exponent}");
            let _ = n(&base).power(&n(&exponent));
            let _ = n(&base).power(&n(&negative));
        }
    }
}
