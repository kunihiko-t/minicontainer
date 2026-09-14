//! Strict JSON value parser for OCI descriptors.
//!
//! The parser accepts the full JSON grammar so unknown fields can be
//! skipped, but rejects duplicate object keys, malformed escapes, and
//! over-deep nesting. Numbers keep their raw literal; callers convert sizes
//! with [`number_as_u64`], which accepts plain non-negative integers only.

use crate::OciError;

/// Maximum nesting depth of parsed values. Genuine descriptors nest five
/// levels deep; anything beyond 64 is hostile input.
const MAX_DEPTH: usize = 64;

/// One parsed JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonValue {
    /// JSON `null`.
    Null,
    /// JSON `true` or `false`.
    Bool(bool),
    /// A number literal in its raw form.
    Number(String),
    /// An unescaped string.
    String(String),
    /// An array of values.
    Array(Vec<JsonValue>),
    /// An object with unique keys in document order.
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    /// Looks up one object member by key.
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            Self::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Borrows the value as a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    /// Borrows the value as an array.
    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Converts a number literal to a plain non-negative integer. Fractions,
    /// exponents, signs, and leading zeros are rejected: descriptor sizes
    /// from conforming tools are plain integers, and anything else indicates
    /// hand-crafted input this reader refuses to guess about.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(raw) => number_as_u64(raw),
            _ => None,
        }
    }
}

/// Parses one complete JSON document. Trailing bytes after the top-level
/// value are rejected.
pub fn parse_json(bytes: &[u8]) -> Result<JsonValue, OciError> {
    let text = std::str::from_utf8(bytes).map_err(|_| OciError::Json {
        message: "document is not UTF-8".to_owned(),
    })?;
    let mut parser = Parser {
        bytes: text.as_bytes(),
        cursor: 0,
    };
    let value = parser.parse_value(0)?;
    parser.skip_whitespace();
    if parser.cursor != parser.bytes.len() {
        return Err(OciError::Json {
            message: format!("trailing bytes at offset {}", parser.cursor),
        });
    }
    Ok(value)
}

/// Converts a raw number literal to `u64` under the plain-integer rule.
fn number_as_u64(raw: &str) -> Option<u64> {
    if raw.is_empty() {
        return None;
    }
    if raw.len() > 1 && raw.starts_with('0') {
        return None;
    }
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// Byte cursor over one JSON document.
struct Parser<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl Parser<'_> {
    /// Parses one value at the current depth.
    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, OciError> {
        if depth > MAX_DEPTH {
            return Err(OciError::Json {
                message: "nesting exceeds the depth limit".to_owned(),
            });
        }
        self.skip_whitespace();
        let byte = self
            .peek()
            .ok_or_else(|| self.error("unexpected end of input"))?;
        match byte {
            b'{' => self.parse_object(depth),
            b'[' => self.parse_array(depth),
            b'"' => Ok(JsonValue::String(self.parse_string()?)),
            b't' => self.parse_literal("true", JsonValue::Bool(true)),
            b'f' => self.parse_literal("false", JsonValue::Bool(false)),
            b'n' => self.parse_literal("null", JsonValue::Null),
            b'-' | b'0'..=b'9' => Ok(JsonValue::Number(self.parse_number()?)),
            _ => Err(self.error("unexpected character")),
        }
    }

    /// Parses one object with duplicate-key rejection.
    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, OciError> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(JsonValue::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("object keys must be strings"));
            }
            let key = self.parse_string()?;
            if members.iter().any(|(name, _)| *name == key) {
                return Err(OciError::Json {
                    message: format!("duplicate object key: {key}"),
                });
            }
            self.skip_whitespace();
            self.expect(b':')?;
            let value = self.parse_value(depth + 1)?;
            members.push((key, value));
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(JsonValue::Object(members));
            }
            self.expect(b',')?;
        }
    }

    /// Parses one array.
    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, OciError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(JsonValue::Array(items));
            }
            self.expect(b',')?;
        }
    }

    /// Parses one string with full escape handling, including `\u` escapes
    /// and surrogate pairs. Raw control bytes are rejected.
    fn parse_string(&mut self) -> Result<String, OciError> {
        self.expect(b'"')?;
        let mut text = String::new();
        loop {
            let byte = self
                .next()
                .ok_or_else(|| self.error("unterminated string"))?;
            match byte {
                b'"' => return Ok(text),
                b'\\' => {
                    let escaped = self
                        .next()
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    match escaped {
                        b'"' => text.push('"'),
                        b'\\' => text.push('\\'),
                        b'/' => text.push('/'),
                        b'b' => text.push('\u{8}'),
                        b'f' => text.push('\u{c}'),
                        b'n' => text.push('\n'),
                        b'r' => text.push('\r'),
                        b't' => text.push('\t'),
                        b'u' => text.push(self.parse_unicode()?),
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                0x00..=0x1f => return Err(self.error("unescaped control in string")),
                _ => {
                    let start = self.cursor - 1;
                    let rest = &self.bytes[start..];
                    let token = rest
                        .iter()
                        .take_while(|byte| **byte >= 0x20 && **byte != b'"' && **byte != b'\\')
                        .count();
                    let chunk =
                        std::str::from_utf8(&rest[..token]).map_err(|_| self.error("bad UTF-8"))?;
                    text.push_str(chunk);
                    self.cursor = start + token;
                }
            }
        }
    }

    /// Parses one `\uXXXX` escape, combining surrogate pairs.
    fn parse_unicode(&mut self) -> Result<char, OciError> {
        let high = self.parse_hex4()?;
        if (0xd800..0xdc00).contains(&high) {
            if self.next() != Some(b'\\') || self.next() != Some(b'u') {
                return Err(self.error("lone high surrogate"));
            }
            let low = self.parse_hex4()?;
            if !(0xdc00..0xe000).contains(&low) {
                return Err(self.error("lone high surrogate"));
            }
            let scalar = 0x1_0000 + ((high - 0xd800) << 10) + (low - 0xdc00);
            return char::from_u32(scalar).ok_or_else(|| self.error("bad scalar"));
        }
        if (0xdc00..0xe000).contains(&high) {
            return Err(self.error("lone low surrogate"));
        }
        char::from_u32(high).ok_or_else(|| self.error("bad scalar"))
    }

    /// Parses four hexadecimal digits.
    fn parse_hex4(&mut self) -> Result<u32, OciError> {
        if self.cursor + 4 > self.bytes.len() {
            return Err(self.error("truncated unicode escape"));
        }
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.next().expect("checked length");
            let digit = (byte as char)
                .to_digit(16)
                .ok_or_else(|| self.error("bad unicode escape"))?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// Parses one number literal with the full JSON grammar.
    fn parse_number(&mut self) -> Result<String, OciError> {
        let start = self.cursor;
        if self.peek() == Some(b'-') {
            self.cursor += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.cursor += 1;
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.cursor += 1;
                }
            }
            _ => return Err(self.error("bad number")),
        }
        if self.peek() == Some(b'.') {
            self.cursor += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("bad number"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.cursor += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.cursor += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.cursor += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("bad number"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.cursor += 1;
            }
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.cursor]).into_owned())
    }

    /// Parses one `true`, `false`, or `null` literal.
    fn parse_literal(&mut self, literal: &str, value: JsonValue) -> Result<JsonValue, OciError> {
        if self.bytes[self.cursor..].starts_with(literal.as_bytes()) {
            self.cursor += literal.len();
            Ok(value)
        } else {
            Err(self.error("bad literal"))
        }
    }

    /// Skips JSON whitespace.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.cursor += 1;
        }
    }

    /// Returns the current byte without consuming it.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }

    /// Consumes and returns the current byte.
    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.cursor += 1;
        Some(byte)
    }

    /// Consumes one expected byte.
    fn expect(&mut self, byte: u8) -> Result<(), OciError> {
        if self.next() == Some(byte) {
            Ok(())
        } else {
            Err(self.error("unexpected character"))
        }
    }

    /// Consumes one byte when it matches.
    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    /// Builds an offset-carrying syntax error.
    fn error(&self, message: &str) -> OciError {
        OciError::Json {
            message: format!("{message} at offset {}", self.cursor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values_and_skips_whitespace() {
        assert_eq!(parse_json(b"null"), Ok(JsonValue::Null));
        assert_eq!(parse_json(b"true"), Ok(JsonValue::Bool(true)));
        assert_eq!(
            parse_json(b" \t\r\n42 "),
            Ok(JsonValue::Number("42".to_owned()))
        );
        assert_eq!(
            parse_json(b"{\"a\":[1,\"x\",null]}"),
            Ok(JsonValue::Object(vec![(
                "a".to_owned(),
                JsonValue::Array(vec![
                    JsonValue::Number("1".to_owned()),
                    JsonValue::String("x".to_owned()),
                    JsonValue::Null,
                ])
            )]))
        );
    }

    #[test]
    fn rejects_trailing_bytes_and_bad_shapes() {
        assert!(matches!(parse_json(b"{}x"), Err(OciError::Json { .. })));
        assert!(matches!(parse_json(b"{,}"), Err(OciError::Json { .. })));
        assert!(matches!(parse_json(b"{\"a\"}"), Err(OciError::Json { .. })));
        assert!(matches!(parse_json(b"[1,]"), Err(OciError::Json { .. })));
        assert!(matches!(parse_json(b"nul"), Err(OciError::Json { .. })));
        assert!(matches!(parse_json(b"\xff"), Err(OciError::Json { .. })));
    }

    #[test]
    fn rejects_duplicate_keys_at_any_level() {
        assert!(matches!(
            parse_json(br#"{"a":1,"a":2}"#),
            Err(OciError::Json { .. })
        ));
        assert!(matches!(
            parse_json(br#"{"a":{"b":1,"b":2}}"#),
            Err(OciError::Json { .. })
        ));
        assert_eq!(
            parse_json(br#"{"a":1,"b":1}"#).unwrap(),
            JsonValue::Object(vec![
                ("a".to_owned(), JsonValue::Number("1".to_owned())),
                ("b".to_owned(), JsonValue::Number("1".to_owned())),
            ])
        );
    }

    #[test]
    fn unescapes_strings_including_surrogate_pairs() {
        assert_eq!(
            parse_json(br#""a\"b\\c/d""#).unwrap(),
            JsonValue::String("a\"b\\c/d".to_owned())
        );
        assert_eq!(
            parse_json(br#""\u0041\u00e9""#).unwrap(),
            JsonValue::String("Aé".to_owned())
        );
        assert_eq!(
            parse_json(br#""\ud83d\ude00""#).unwrap(),
            JsonValue::String("😀".to_owned())
        );
        assert!(matches!(
            parse_json(br#""\ud83d""#),
            Err(OciError::Json { .. })
        ));
        assert!(matches!(
            parse_json(b"\"\x01\""),
            Err(OciError::Json { .. })
        ));
        assert!(matches!(parse_json(br#""\x""#), Err(OciError::Json { .. })));
    }

    #[test]
    fn sizes_accept_plain_integers_only() {
        assert_eq!(JsonValue::Number("0".to_owned()).as_u64(), Some(0));
        assert_eq!(
            JsonValue::Number("18446744073709551615".to_owned()).as_u64(),
            Some(u64::MAX)
        );
        for raw in ["", "01", "-1", "1.0", "1e2", "18446744073709551616", "x"] {
            assert_eq!(JsonValue::Number(raw.to_owned()).as_u64(), None, "{raw}");
        }
        assert_eq!(JsonValue::String("1".to_owned()).as_u64(), None);
    }

    #[test]
    fn rejects_deep_nesting() {
        let deep = format!("{}{}{}", "[".repeat(65), "1", "]".repeat(65));
        assert!(matches!(
            parse_json(deep.as_bytes()),
            Err(OciError::Json { .. })
        ));
        let shallow = format!("{}{}{}", "[".repeat(8), "1", "]".repeat(8));
        assert!(parse_json(shallow.as_bytes()).is_ok());
    }
}
