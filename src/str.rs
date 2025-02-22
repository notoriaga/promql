use crate::utils::IResult;
use nom::branch::alt;
use nom::bytes::complete::{is_not, take};
use nom::character::complete::char;
use nom::combinator::{map, map_opt, map_res};
use nom::multi::many0;
use nom::sequence::{delimited, preceded};
use nom::{
    AsBytes, AsChar, FindToken, InputIter, InputLength, InputTake, InputTakeAtPosition, Slice,
};
use std::ops::RangeFrom;

// > Label values may contain any Unicode characters.
// > PromQL follows the same [escaping rules as Go](https://golang.org/ref/spec#String_literals).

// XXX is it an overkill to employ quick-error just to couple two error types that user wouldn't even see?
quick_error! {
    #[derive(Debug)]
    pub enum UnicodeRuneError {
        UTF8(err: ::std::string::FromUtf8Error) {
            from()
        }
        Int(err: ::std::num::ParseIntError) {
            from()
        }
    }
}

// `fixed_length_radix!(T, n, radix)` parses sequence of `n` chars as a `radix`-base number into a type `T`
// cannot be implemented as function since `from_str_radix` is not a part of any trait and is implemented directly for every primitive type
macro_rules! fixed_length_radix {
    // $type is :ident, not :ty; otherwise "error: expected expression, found `u8`" in "$type::from_str_radix"
    ($input:ty, $type:ident, $len:expr, $radix:expr) => {
        // there's no easy way to combine nom::is_(whatever)_digit with something like length_count
        // besides u123::from_str_radix will validate chars anyways, so why do extra work?
        map_res(take($len), |n: $input| -> Result<_, UnicodeRuneError> {
            Ok($type::from_str_radix(
                &String::from_utf8(n.as_bytes().to_vec())?,
                $radix,
            )?)
        })
    };
}

// go does not allow invalid unicode scalars (surrogates, chars beyond U+10ffff), and the same applies to from_u32()
fn validate_unicode_scalar(n: u32) -> Option<Vec<u8>> {
    ::std::char::from_u32(n).map(|c| {
        let mut tmp = [0; 4];
        c.encode_utf8(&mut tmp).as_bytes().to_vec()
    })
}

fn rune<I, C>(input: I) -> IResult<I, Vec<u8>>
where
    I: Clone + AsBytes + InputIter<Item = C> + InputTake + Slice<RangeFrom<usize>>,
    C: AsChar,
{
    preceded(
        char('\\'),
        alt((
            // not using value() here to avoid allocation of lots of temporary Vec *per rune() call*
            map(char('a'), |_| vec![0x07]),
            map(char('b'), |_| vec![0x08]),
            map(char('f'), |_| vec![0x0c]),
            map(char('n'), |_| vec![0x0a]),
            map(char('r'), |_| vec![0x0d]),
            map(char('t'), |_| vec![0x09]),
            map(char('v'), |_| vec![0x0b]),
            // TODO? should we really care whether \' is used in ""-strings or vice versa? (Prometheus itself does…)
            map(char('\\'), |_| vec![0x5c]),
            map(char('\''), |_| vec![0x27]),
            map(char('"'), |_| vec![0x22]),
            map(fixed_length_radix!(I, u8, 3u8, 8), |n| vec![n]),
            map(
                preceded(char('x'), fixed_length_radix!(I, u8, 2u8, 16)),
                |n| vec![n],
            ),
            map_opt(
                preceded(char('u'), fixed_length_radix!(I, u32, 4u8, 16)),
                validate_unicode_scalar,
            ),
            map_opt(
                preceded(char('U'), fixed_length_radix!(I, u32, 8u8, 16)),
                validate_unicode_scalar,
            ),
        )),
    )(input)
}

// parses sequence of chars that are not in arg
// returns Vec<u8> (unlike none_of!() which returns &[char], or is_not!() which returns &[u8])
fn is_not_v<I, C>(arg: &'static str) -> impl FnMut(I) -> IResult<I, Vec<u8>>
where
    I: Clone
        + AsBytes
        + InputIter<Item = C>
        + InputLength
        + InputTake
        + InputTakeAtPosition<Item = C>,
    &'static str: FindToken<C>,
{
    map(is_not(arg), |bytes: I| bytes.as_bytes().to_vec())
}

// sequence of chars (except those marked as invalid in $arg) or rune literals, parsed into Vec<u8>
fn chars_except<I, C>(arg: &'static str) -> impl FnMut(I) -> IResult<I, Vec<u8>>
where
    I: Clone
        + AsBytes
        + InputIter<Item = C>
        + InputLength
        + InputTake
        + InputTakeAtPosition<Item = C>
        + Slice<RangeFrom<usize>>,
    C: AsChar,
    &'static str: FindToken<C>,
{
    map(many0(alt((rune, is_not_v(arg)))), |s| s.concat())
}

// Go and PromQL allow arbitrary byte sequences, hence Vec<u8> instead of String
pub fn string<I, C>(input: I) -> IResult<I, Vec<u8>>
where
    I: Clone
        + AsBytes
        + InputIter<Item = C>
        + InputLength
        + InputTake
        + InputTakeAtPosition<Item = C>
        + Slice<RangeFrom<usize>>,
    C: AsChar,
    &'static str: FindToken<C>,
{
    alt((
        // newlines are not allowed in interpreted quotes, but are totally fine in raw string literals
        delimited(char('"'), chars_except("\n\"\\"), char('"')),
        delimited(char('\''), chars_except("\n'\\"), char('\'')),
        // raw string literals, where "backslashes have no special meaning"
        delimited(char('`'), is_not_v("`"), char('`')),
    ))(input)
}

#[allow(unused_imports)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::tests::*;
    use nom::error::{ErrorKind, VerboseErrorKind};

    #[test]
    fn strings() {
        assert_eq!(
            string(r#""lorem ipsum \"dolor\nsit amet\"""#),
            Ok(("", b"lorem ipsum \"dolor\nsit amet\"".to_vec()))
        );

        assert_eq!(
            string(r#"'lorem ipsum \'dolor\nsit\tamet\''"#),
            Ok(("", b"lorem ipsum 'dolor\nsit\tamet'".to_vec()))
        );

        assert_eq!(
            string(r#"`lorem ipsum \"dolor\nsit\tamet\"`"#),
            Ok((
                "",
                br#"lorem ipsum \"dolor\nsit\tamet\""#.to_vec() // N.B. r# literal
            ))
        );

        // literal, non-escaped newlines

        assert_eq!(
            string("'this\nis not valid'"),
            err(vec![
                ("'this\nis not valid'", VerboseErrorKind::Char('`'),),
                (
                    "'this\nis not valid'",
                    VerboseErrorKind::Nom(ErrorKind::Alt),
                ),
            ])
        );

        assert_eq!(string("`but this\nis`"), Ok(("", b"but this\nis".to_vec())));

        // strings with runes

        for s in [
            r#"'inf: ∞'"#,
            r#"'inf: \u221e'"#,
            r#"'inf: \u221E'"#,
            r#"'inf: \U0000221e'"#,
            r#"'inf: \U0000221E'"#,
            r#"'inf: \xe2\x88\x9e'"#,
            r#"'inf: \xE2\x88\x9E'"#,
        ] {
            assert_eq!(string(s), Ok(("", b"inf: \xe2\x88\x9e".to_vec())));
        }

        for s in [
            r#"'thinking: 🤔'"#,
            r#"'thinking: \U0001f914'"#,
            r#"'thinking: \U0001F914'"#,
            r#"'thinking: \xf0\x9f\xa4\x94'"#,
            r#"'thinking: \xF0\x9F\xA4\x94'"#,
        ] {
            assert_eq!(string(s), Ok(("", b"thinking: \xf0\x9f\xa4\x94".to_vec())));
        }
    }

    #[test]
    fn runes() {
        assert_eq!(rune("\\123"), Ok(("", vec![0o123])));

        assert_eq!(rune("\\x23"), Ok(("", vec![0x23])));

        assert_eq!(rune("\\uabcd"), Ok(("", "\u{abcd}".as_bytes().to_vec())));

        // high surrogate
        assert_eq!(
            rune("\\uD801"),
            err(vec![
                ("uD801", VerboseErrorKind::Char('U')),
                ("uD801", VerboseErrorKind::Nom(ErrorKind::Alt)),
            ])
        );

        assert_eq!(
            rune("\\U00010330"),
            Ok(("", "\u{10330}".as_bytes().to_vec()))
        );

        // out of range
        assert_eq!(
            rune("\\UdeadDEAD"),
            err(vec![
                ("UdeadDEAD", VerboseErrorKind::Nom(ErrorKind::MapOpt)),
                ("UdeadDEAD", VerboseErrorKind::Nom(ErrorKind::Alt)),
            ]),
        );

        // utter nonsense

        assert_eq!(
            rune("\\xxx"),
            err(vec![
                ("xxx", VerboseErrorKind::Char('U')),
                ("xxx", VerboseErrorKind::Nom(ErrorKind::Alt)),
            ]),
        );

        assert_eq!(
            rune("\\x1"),
            err(vec![
                ("x1", VerboseErrorKind::Char('U')),
                ("x1", VerboseErrorKind::Nom(ErrorKind::Alt)),
            ]),
        );
    }
}
