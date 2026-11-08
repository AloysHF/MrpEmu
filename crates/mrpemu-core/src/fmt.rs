//! Minimal C-style printf formatting for the guest's `mr_sprintf` and
//! `mr_printf` system calls.
//!
//! Supports the conversions MRP games actually use (`%d`, `%i`, `%u`, `%x`,
//! `%X`, `%s`, `%c`, `%%`) plus `%ld`/`%lu`/`%hd` aliases, zero padding, and
//! a decimal width field (`%02d`, `%4x`, …). Unrecognized conversions leave
//! the `%` literal, mirroring the tolerant behavior of the reference
//! runtime's formatter.

/// One argument resolved lazily from the guest, in positional order.
#[derive(Debug, Clone)]
pub enum Arg {
    Num(u32),
    Str(Vec<u8>),
    /// Argument slot consumed by a conversion the formatter ignores.
    Skip,
}

/// Formats `format`, calling `resolve` for each conversion in order. The
/// resolver receives the argument's positional index and the conversion
/// byte, so callers that must interpret arguments lazily (e.g. reading
/// guest strings) can branch on the conversion type.
pub fn format(format: &[u8], mut resolve: impl FnMut(usize, u8) -> Arg) -> Vec<u8> {
    let mut out = Vec::with_capacity(format.len() + 16);
    let mut arg_index = 0usize;
    let mut i = 0usize;

    while i < format.len() {
        let byte = format[i];
        if byte != b'%' {
            out.push(byte);
            i += 1;
            continue;
        }
        let Some(spec) = parse_spec(format, i + 1) else {
            out.push(b'%');
            i += 1;
            continue;
        };
        i = spec.end;
        if spec.conversion == b'%' {
            out.push(b'%');
            continue;
        }

        match resolve(arg_index, spec.conversion) {
            Arg::Num(arg) => {
                arg_index += 1;
                match spec.conversion {
                    b'd' | b'i' => pad(&mut out, &signed_decimal(arg as i32), &spec),
                    b'u' => pad(&mut out, arg.to_string().into_bytes().as_slice(), &spec),
                    b'x' | b'X' => pad(&mut out, &hex(arg, spec.conversion == b'X'), &spec),
                    b'c' => pad(&mut out, &[arg as u8], &spec),
                    _ => {}
                }
            }
            Arg::Str(value) => {
                arg_index += 1;
                if spec.conversion == b's' {
                    pad(&mut out, &value, &spec);
                }
            }
            Arg::Skip => {}
        }
    }
    out
}

struct Spec {
    end: usize,
    zero_pad: bool,
    width: usize,
    conversion: u8,
}

/// Parses a conversion spec starting right after the `%`; returns `None`
/// when the sequence is not a conversion (the `%` is then literal).
fn parse_spec(format: &[u8], start: usize) -> Option<Spec> {
    let mut i = start;
    let mut zero_pad = false;
    if i < format.len() && format[i] == b'0' {
        zero_pad = true;
        i += 1;
    }
    let mut width = 0usize;
    let mut has_width = false;
    while i < format.len() && format[i].is_ascii_digit() {
        has_width = true;
        width = width * 10 + (format[i] - b'0') as usize;
        i += 1;
    }
    // Length modifiers (`l`, `h`) are accepted and skipped.
    while i < format.len() && matches!(format[i], b'l' | b'h') {
        i += 1;
    }
    let conversion = *format.get(i)?;
    if !matches!(
        conversion,
        b'd' | b'i' | b'u' | b'x' | b'X' | b's' | b'c' | b'%'
    ) {
        return None;
    }
    Some(Spec {
        end: i + 1,
        zero_pad: zero_pad && has_width,
        width,
        conversion,
    })
}

fn signed_decimal(value: i32) -> Vec<u8> {
    let mut digits = (value.unsigned_abs()).to_string().into_bytes();
    if value < 0 {
        digits.insert(0, b'-');
    }
    digits
}

fn hex(value: u32, upper: bool) -> Vec<u8> {
    let table = if upper {
        b"0123456789ABCDEF" as &[u8]
    } else {
        b"0123456789abcdef" as &[u8]
    };
    let mut out = Vec::with_capacity(8);
    let mut value = value;
    if value == 0 {
        out.push(b'0');
    }
    while value != 0 {
        out.push(table[(value & 0xF) as usize]);
        value >>= 4;
    }
    out.reverse();
    out
}

fn pad(out: &mut Vec<u8>, value: &[u8], spec: &Spec) {
    let fill = if spec.zero_pad { b'0' } else { b' ' };
    for _ in value.len()..spec.width {
        out.push(fill);
    }
    out.extend_from_slice(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(format: &[u8], args: &[Arg]) -> String {
        String::from_utf8(super::format(format, |i, _conv| {
            args.get(i).cloned().unwrap_or(Arg::Skip)
        }))
        .expect("test output is ascii")
    }

    #[test]
    fn formats_decimal_hex_and_percent() {
        assert_eq!(fmt(b"n=%d", &[Arg::Num(7)]), "n=7");
        assert_eq!(fmt(b"n=%d", &[Arg::Num(-7i32 as u32)]), "n=-7");
        assert_eq!(fmt(b"%u%%", &[Arg::Num(42)]), "42%");
        assert_eq!(
            fmt(b"%x %X", &[Arg::Num(0xabcd), Arg::Num(0xabcd)]),
            "abcd ABCD"
        );
        assert_eq!(fmt(b"%02x", &[Arg::Num(5)]), "05");
        assert_eq!(fmt(b"%4d|", &[Arg::Num(9)]), "   9|");
        assert_eq!(fmt(b"%ld", &[Arg::Num(123)]), "123");
    }

    #[test]
    fn formats_strings_chars_and_mixed_args() {
        let out = format(b"%s=%d (%c)", |i, _conv| match i {
            0 => Arg::Str(b"count".to_vec()),
            1 => Arg::Num(3),
            2 => Arg::Num(u32::from(b'x')),
            _ => Arg::Skip,
        });
        assert_eq!(String::from_utf8(out).unwrap(), "count=3 (x)");
    }

    #[test]
    fn resolver_sees_the_conversion_byte() {
        // The resolver branches on the conversion: %s reads a string, %d a
        // number — from the same positional stream.
        let out = format(b"%d/%s", |i, conv| match (i, conv) {
            (0, b'd') => Arg::Num(9),
            (1, b's') => Arg::Str(b"nine".to_vec()),
            _ => Arg::Skip,
        });
        assert_eq!(String::from_utf8(out).unwrap(), "9/nine");
    }

    #[test]
    fn literal_percent_and_unknown_spec_stays_literal() {
        assert_eq!(fmt(b"100%%", &[]), "100%");
        // An unrecognized conversion falls back to a literal '%'.
        assert_eq!(fmt(b"a%q b", &[]), "a%q b");
    }
}
