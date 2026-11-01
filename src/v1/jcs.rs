//! JSON Canonicalization Scheme (RFC 8785) for the objects the mini app
//! protocol signs: session certificates, call envelopes and permissions
//! digests. The broker's canonicalizer (`server/v1/jcs.mjs`) must produce
//! the same bytes, so both are tested against one set of vectors
//! (`jcs-vectors.json`, copied here from grok-shell).
//!
//! Signed objects carry integers only (spec 4, design §5): a number that is
//! not a safe integer is refused rather than trusting two float printers to
//! agree. `1.0` and `1e3` are integers written another way, as in JS.
//! Received JSON is parsed as I-JSON ([`parse_ijson`]): a duplicate key is
//! refused, where `serde_json` would keep the last one.

use serde_json::Value;

/// The largest integer JS represents exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

/// RFC 8785 text of `v`, refusing any number that is not a safe integer.
pub fn canonical(v: &Value) -> Result<String, String> {
    let mut out = String::new();
    write(v, &mut out)?;
    Ok(out)
}

/// The UTF-8 bytes a signature covers: [`canonical`]'s.
pub fn signed_bytes(v: &Value) -> Result<Vec<u8>, String> {
    canonical(v).map(String::into_bytes)
}

fn write(v: &Value, out: &mut String) -> Result<(), String> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&integer(n)?),
        Value::String(s) => string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // keys sort by UTF-16 code units (RFC 8785 3.2.3), not by bytes
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(k, out);
                out.push(':');
                write(&map[k], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// A safe integer's decimal text; `-0` is `0`.
fn integer(n: &serde_json::Number) -> Result<String, String> {
    if let Some(i) = n.as_i64() {
        return safe(i as f64).map(|_| i.to_string());
    }
    if let Some(u) = n.as_u64() {
        return safe(u as f64).map(|_| u.to_string());
    }
    let f = n.as_f64().unwrap_or(f64::NAN);
    safe(f)?;
    if f.fract() != 0.0 {
        return Err(format!("signed objects carry integers only, not {n}"));
    }
    Ok(format!("{}", f as i64))
}

fn safe(f: f64) -> Result<(), String> {
    if f.is_finite() && f.abs() <= MAX_SAFE && f.fract() == 0.0 {
        Ok(())
    } else {
        Err(format!("signed objects carry safe integers only, not {f}"))
    }
}

/// A string as `JSON.stringify` writes it: `"` and `\` escaped, the
/// control characters as `\b \t \n \f \r` or `\u00xx`, the rest as is.
fn string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parses I-JSON (RFC 7493): JSON with no duplicate key at any depth and
/// no lone surrogate (which `serde_json` refuses already).
pub fn parse_ijson(text: &str) -> Result<Value, String> {
    let mut de = serde_json::Deserializer::from_str(text);
    let v = serde::Deserialize::deserialize(&mut de).map(|Strict(v)| v).map_err(|e| e.to_string())?;
    de.end().map_err(|e| e.to_string())?;
    Ok(v)
}

/// A JSON value whose objects refuse a key they already have.
struct Strict(Value);

impl<'de> serde::Deserialize<'de> for Strict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> serde::de::Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
        Ok(Value::Bool(b))
    }

    fn visit_i64<E>(self, n: i64) -> Result<Value, E> {
        Ok(n.into())
    }

    fn visit_u64<E>(self, n: u64) -> Result<Value, E> {
        Ok(n.into())
    }

    fn visit_f64<E>(self, n: f64) -> Result<Value, E> {
        Ok(serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, s: &str) -> Result<Value, E> {
        Ok(Value::String(s.to_owned()))
    }

    fn visit_string<E>(self, s: String) -> Result<Value, E> {
        Ok(Value::String(s))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(v)) = seq.next_element()? {
            items.push(v);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(k) = map.next_key::<String>()? {
            let Strict(v) = map.next_value()?;
            if out.insert(k.clone(), v).is_some() {
                return Err(serde::de::Error::custom(format!("duplicate key {k:?}")));
            }
        }
        Ok(Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    /// The vectors grok-shell's `server/v1/jcs.mjs` is tested against.
    const VECTORS: &str = include_str!("jcs-vectors.json");

    #[test]
    fn jcs_matches_the_shared_vectors() {
        let all: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        let vectors = all["vectors"].as_array().unwrap();
        assert!(vectors.len() >= 15, "{} vectors", vectors.len());
        for v in vectors {
            let name = v["name"].as_str().unwrap();
            let parsed = super::parse_ijson(v["input"].as_str().unwrap());
            if v["error"] == true {
                assert!(parsed.is_err(), "{name}: not I-JSON, parsed");
                continue;
            }
            let parsed = parsed.unwrap_or_else(|e| panic!("{name}: {e}"));
            let out = super::canonical(&parsed);
            if v["signable"] == true {
                assert_eq!(out.as_deref(), Ok(v["jcs"].as_str().unwrap()), "{name}");
            } else {
                assert!(out.is_err(), "{name}: signed a non-integer: {out:?}");
            }
        }
    }

    #[test]
    fn jcs_refuses_floats_and_unsafe_integers() {
        use serde_json::json;
        assert!(super::canonical(&json!({"a": 0.5})).is_err());
        assert!(super::canonical(&json!([9007199254740992u64])).is_err());
        assert!(super::canonical(&json!([-9007199254740992i64])).is_err());
        assert_eq!(super::canonical(&json!([1.0, -0.0, 1e3])).unwrap(), "[1,0,1000]");
    }
}
