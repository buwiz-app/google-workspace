//! The few JavaScript conversions server.mjs leans on (`String(v)`,
//! `Number(v)`, `trim()`, `slice()`), so the answers match it byte for byte.

use serde_json::Value;

/// What `\s` and `trim()` count as space.
pub fn is_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

/// `s.trim()`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `s.slice(0, n)`: n UTF-16 units. A pair cut in half leaves U+FFFD where
/// Node would leave a lone surrogate (which JSON can't carry to Rust).
pub fn slice_utf16(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in s.chars() {
        let len = c.len_utf16();
        if units + len > n {
            if units < n {
                out.push('\u{fffd}');
            }
            break;
        }
        units += len;
        out.push(c);
    }
    out
}

/// `s.length`.
pub fn len_utf16(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `String(n)`: shortest round-trip digits, as JavaScript prints them.
pub fn number(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_owned();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if f == 0.0 {
        return "0".to_owned();
    }
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().unwrap_or(0) + 1;
    let s = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        let head = if k == 1 { digits.clone() } else { format!("{}.{}", &digits[..1], &digits[1..]) };
        format!("{head}e{sign}{}", e.abs())
    };
    if f < 0.0 {
        format!("-{s}")
    } else {
        s
    }
}

/// `String(v)`; `None` is `undefined`.
pub fn string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_owned(),
        Some(Value::Null) => "null".to_owned(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => number(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().map(|i| if i.is_null() { String::new() } else { string(Some(i)) }).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".to_owned(),
    }
}

/// `String(v ?? "")`.
pub fn string_or_empty(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        v => string(v),
    }
}

/// `Number(v)`; `None` is `undefined` (NaN).
pub fn to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => string_to_number(s),
        Some(v @ Value::Array(_)) => string_to_number(&string(Some(v))),
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// `Number(s)` of a string: a decimal or 0x/0o/0b literal, or Infinity,
/// with space around it; empty is 0; anything else NaN.
pub fn string_to_number(s: &str) -> f64 {
    let t = trim(s);
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if rest.is_empty() || !rest.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return rest.chars().fold(0.0, |n, c| n * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0)));
        }
    }
    let (sign, body) = match t.as_bytes()[0] {
        b'-' => (-1.0, &t[1..]),
        b'+' => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    // [digits][.digits][e[+-]digits], at least one digit before the exponent
    let b = body.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    let mut mantissa = digits(&mut i);
    if i < b.len() && b[i] == b'.' {
        i += 1;
        mantissa += digits(&mut i);
    }
    if mantissa == 0 {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return f64::NAN;
        }
    }
    if i != b.len() {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// `Math.round(x)`: to the nearest integer, halves up.
pub fn round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// JavaScript truthiness.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn number_and_string_convert_as_javascript_does() {
        for (v, n) in [
            (json!("5"), 5.0),
            (json!(" 2.5 "), 2.5),
            (json!(""), 0.0),
            (json!("0x10"), 16.0),
            (json!([7]), 7.0),
            (json!([]), 0.0),
            (json!(null), 0.0),
            (json!(true), 1.0),
            (json!("1e3"), 1000.0),
            (json!(".5"), 0.5),
            (json!("5."), 5.0),
        ] {
            assert_eq!(to_number(Some(&v)), n, "{v}");
        }
        for v in [json!("abc"), json!("1,2"), json!([1, 2]), json!({}), json!("inf"), json!("NaN"), json!("1e"), json!("."), json!("-0x1")] {
            assert!(to_number(Some(&v)).is_nan(), "{v}");
        }
        assert!(to_number(None).is_nan());
        assert_eq!(string(Some(&json!([1, null, "z"]))), "1,,z");
        assert_eq!(string(Some(&json!(12.5))), "12.5");
        assert_eq!(string(Some(&json!(1e21))), "1e+21");
        assert_eq!(string(None), "undefined");
        assert_eq!((round(2.5), round(-2.5), round(0.49999999999999994)), (3.0, -2.0, 0.0));
    }

    #[test]
    fn slices_count_utf16_units() {
        assert_eq!(slice_utf16("ab😀c", 3), "ab\u{fffd}");
        assert_eq!(slice_utf16("ab😀c", 4), "ab😀");
        assert_eq!(len_utf16("😀é"), 3);
        assert_eq!(trim("\u{feff} x \u{a0}\n"), "x");
        assert_eq!(trim("\u{85}x"), "\u{85}x");
    }
}
