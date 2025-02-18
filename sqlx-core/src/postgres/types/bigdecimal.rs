use std::cmp;
use std::convert::{TryFrom, TryInto};

use bigdecimal::{BigDecimal, ToPrimitive, Zero};
use num_bigint::{BigInt, Sign};

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::postgres::types::numeric::{PgNumeric, PgNumericSign};
use crate::postgres::{PgArgumentBuffer, PgTypeInfo, PgValueFormat, PgValueRef, Postgres};
use crate::types::Type;

impl Type<Postgres> for BigDecimal {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::NUMERIC
    }
}

impl Type<Postgres> for [BigDecimal] {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::NUMERIC_ARRAY
    }
}

impl Type<Postgres> for Vec<BigDecimal> {
    fn type_info() -> PgTypeInfo {
        <[BigDecimal] as Type<Postgres>>::type_info()
    }
}

impl TryFrom<PgNumeric> for BigDecimal {
    type Error = BoxDynError;

    fn try_from(numeric: PgNumeric) -> Result<Self, BoxDynError> {
        let (digits, sign, weight) = match numeric {
            PgNumeric::Number {
                digits,
                sign,
                weight,
                ..
            } => (digits, sign, weight),

            PgNumeric::NotANumber => {
                return Err("BigDecimal does not support NaN values".into());
            }
        };

        if digits.is_empty() {
            // Postgres returns an empty digit array for 0 but BigInt expects at least one zero
            return Ok(0u64.into());
        }

        let sign = match sign {
            PgNumericSign::Positive => Sign::Plus,
            PgNumericSign::Negative => Sign::Minus,
        };

        // weight is 0 if the decimal point falls after the first base-10000 digit
        let scale = (digits.len() as i64 - weight as i64 - 1) * 4;

        // no optimized algorithm for base-10 so use base-100 for faster processing
        let mut cents = Vec::with_capacity(digits.len() * 2);
        for digit in &digits {
            cents.push((digit / 100) as u8);
            cents.push((digit % 100) as u8);
        }

        let bigint = BigInt::from_radix_be(sign, &cents, 100)
            .ok_or("PgNumeric contained an out-of-range digit")?;

        Ok(BigDecimal::new(bigint, scale))
    }
}

impl TryFrom<&'_ BigDecimal> for PgNumeric {
    type Error = BoxDynError;

    fn try_from(decimal: &BigDecimal) -> Result<Self, BoxDynError> {
        if decimal.is_zero() {
            return Ok(PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![],
            });
        }

        // NOTE: this unfortunately copies the BigInt internally
        let (integer, exp) = decimal.as_bigint_and_exponent();

        // scale is only nonzero when we have fractional digits
        // since `exp` is the _negative_ decimal exponent, it tells us
        // exactly what our scale should be
        let scale: i16 = cmp::max(0, exp).try_into()?;

        let (sign, uint) = integer.into_parts();
        let mut mantissa = uint.to_u128().unwrap();

        // If our scale is not a multiple of 4, we need to go to the next
        // multiple.
        let groups_diff = scale % 4;
        if groups_diff > 0 {
            let remainder = 4 - groups_diff as u32;
            let power = 10u32.pow(remainder as u32) as u128;

            mantissa = mantissa * power;
        }

        // Array to store max mantissa of Decimal in Postgres decimal format.
        let mut digits = Vec::with_capacity(8);

        // Convert to base-10000.
        while mantissa != 0 {
            digits.push((mantissa % 10_000) as i16);
            mantissa /= 10_000;
        }

        // Change the endianness.
        digits.reverse();

        // Weight is number of digits on the left side of the decimal.
        let digits_after_decimal = (scale + 3) as u16 / 4;
        let weight = digits.len() as i16 - digits_after_decimal as i16 - 1;

        // Remove non-significant zeroes.
        while let Some(&0) = digits.last() {
            digits.pop();
        }

        let sign = match sign {
            Sign::Plus | Sign::NoSign => PgNumericSign::Positive,
            Sign::Minus => PgNumericSign::Negative,
        };

        Ok(PgNumeric::Number {
            sign,
            scale,
            weight,
            digits,
        })
    }
}

/// ### Panics
/// If this `BigDecimal` cannot be represented by [PgNumeric].
impl Encode<'_, Postgres> for BigDecimal {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> IsNull {
        PgNumeric::try_from(self)
            .expect("BigDecimal magnitude too great for Postgres NUMERIC type")
            .encode(buf);

        IsNull::No
    }

    fn size_hint(&self) -> usize {
        // BigDecimal::digits() gives us base-10 digits, so we divide by 4 to get base-10000 digits
        // and since this is just a hint we just always round up
        8 + (self.digits() / 4 + 1) as usize * 2
    }
}

impl Decode<'_, Postgres> for BigDecimal {
    fn decode(value: PgValueRef<'_>) -> Result<Self, BoxDynError> {
        match value.format() {
            PgValueFormat::Binary => PgNumeric::decode(value.as_bytes()?)?.try_into(),
            PgValueFormat::Text => Ok(value.as_str()?.parse::<BigDecimal>()?),
        }
    }
}

#[cfg(test)]
mod bigdecimal_to_pgnumeric {
    use super::{BigDecimal, PgNumeric, PgNumericSign};
    use std::convert::TryFrom;

    #[test]
    fn zero() {
        let zero: BigDecimal = "0".parse().unwrap();

        assert_eq!(
            PgNumeric::try_from(&zero).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![]
            }
        );
    }

    #[test]
    fn one() {
        let one: BigDecimal = "1".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![1]
            }
        );
    }

    #[test]
    fn ten() {
        let ten: BigDecimal = "10".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&ten).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![10]
            }
        );
    }

    #[test]
    fn one_hundred() {
        let one_hundred: BigDecimal = "100".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one_hundred).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![100]
            }
        );
    }

    #[test]
    fn ten_thousand() {
        // BigDecimal doesn't normalize here
        let ten_thousand: BigDecimal = "10000".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&ten_thousand).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 1,
                digits: vec![1]
            }
        );
    }

    #[test]
    fn two_digits() {
        let two_digits: BigDecimal = "12345".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&two_digits).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 1,
                digits: vec![1, 2345]
            }
        );
    }

    #[test]
    fn one_tenth() {
        let one_tenth: BigDecimal = "0.1".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one_tenth).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 1,
                weight: -1,
                digits: vec![1000]
            }
        );
    }

    #[test]
    fn decimal_1() {
        let decimal: BigDecimal = "1.2345".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 4,
                weight: 0,
                digits: vec![1, 2345]
            }
        );
    }

    #[test]
    fn decimal_2() {
        let decimal: BigDecimal = "0.12345".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 5,
                weight: -1,
                digits: vec![1234, 5000]
            }
        );
    }

    #[test]
    fn decimal_3() {
        let decimal: BigDecimal = "0.01234".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 5,
                weight: -1,
                digits: vec![0123, 4000]
            }
        );
    }

    #[test]
    fn decimal_4() {
        let decimal: BigDecimal = "12345.67890".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 5,
                weight: 1,
                digits: vec![1, 2345, 6789]
            }
        );
    }

    #[test]
    fn one_digit_decimal() {
        let one_digit_decimal: BigDecimal = "0.00001234".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one_digit_decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 8,
                weight: -2,
                digits: vec![1234]
            }
        );
    }

    #[test]
    fn issue_423_four_digit() {
        // This is a regression test for https://github.com/launchbadge/sqlx/issues/423
        let four_digit: BigDecimal = "1234".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&four_digit).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![1234]
            }
        );
    }

    #[test]
    fn issue_423_negative_four_digit() {
        // This is a regression test for https://github.com/launchbadge/sqlx/issues/423
        let negative_four_digit: BigDecimal = "-1234".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&negative_four_digit).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Negative,
                scale: 0,
                weight: 0,
                digits: vec![1234]
            }
        );
    }

    #[test]
    fn issue_423_eight_digit() {
        // This is a regression test for https://github.com/launchbadge/sqlx/issues/423
        let eight_digit: BigDecimal = "12345678".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&eight_digit).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 1,
                digits: vec![1234, 5678]
            }
        );
    }

    #[test]
    fn issue_423_negative_eight_digit() {
        // This is a regression test for https://github.com/launchbadge/sqlx/issues/423
        let negative_eight_digit: BigDecimal = "-12345678".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&negative_eight_digit).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Negative,
                scale: 0,
                weight: 1,
                digits: vec![1234, 5678]
            }
        );
    }
}
