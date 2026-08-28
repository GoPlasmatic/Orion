//! A JSON front end that keeps what `serde_json::Value` throws away: the
//! author's key order, where every node sits in the source, and the exact
//! spelling of every number.
//!
//! `serde_json::Value` is the runtime's view of a document and the right one
//! for everything that *executes* it. It is the wrong one for a tool that
//! writes the document back out: its map is a `BTreeMap`, so `"tasks"` comes
//! before `"workflow_id"` whatever the author wrote; it carries no offsets, so
//! a finding can name `tasks[1].condition` but not line 41; and `1.0` becomes
//! the float `1.0`, which prints as `1.0` today and as whatever a future
//! serializer prefers tomorrow. `orion-server fmt` needs all three, and a
//! `file:line:col` prefix on `lint` findings needs the second.
//!
//! So this module parses to its own tree. Nothing else changes: every
//! existing consumer keeps `serde_json::Value`, and [`Document::to_value`] is
//! the bridge — the formatter's correctness guarantee is precisely that a
//! document and its formatted output convert to *equal* values.
//!
//! ## Strictness
//!
//! RFC 8259, and nothing more. A document this parser accepts is one
//! `serde_json` accepts, and vice versa, with two deliberate exceptions that
//! are both *stricter* here:
//!
//! - **a duplicate key is an error.** `serde_json` keeps the last silently,
//!   so the runtime runs something other than what the author sees. A
//!   formatter that re-emitted both would be preserving a bug and one that
//!   dropped one would be choosing; refusing, naming the key and both
//!   offsets, is the only honest option;
//! - **nesting deeper than [`MAX_DEPTH`] is an error** at exactly the depth
//!   `serde_json` refuses, so `fmt` never accepts a document the admin API
//!   will reject. The limit is also what bounds the parser's recursion: a
//!   pathological `[[[[…` fails with [`ParseErrorKind::TooDeep`] long before
//!   it could touch the stack.
//!
//! And one exception in the *lenient* direction: a leading UTF-8 byte-order
//! mark is stripped before parsing. `serde_json` refuses a BOM, so the admin
//! API and `lint` refuse a file that carries one; `fmt` accepts it and writes
//! it back without — repairing the file rather than reporting it. The
//! [`Document::source`] is the text *after* stripping, and every offset is
//! relative to it.

use std::fmt;

/// The deepest nesting `serde_json` accepts — `[[…]]` with 127 opening
/// brackets parses, 128 does not (its recursion budget is 128, spent on
/// entry). Matched exactly so the two parsers agree on every document.
pub const MAX_DEPTH: usize = 127;

/// Byte offsets into [`Document::source`], `start` inclusive, `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A node with the span of the source that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

/// One JSON value, in author order.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    /// The lexeme as written — `1.0`, `1e3`, `-0` — already validated against
    /// the JSON number grammar *and* accepted by `serde_json::Number`, so it
    /// is never re-parsed on the way out.
    Number(Box<str>),
    /// Decoded: escapes resolved, surrogate pairs combined.
    String(String),
    Array(Vec<Spanned<Node>>),
    /// Members in the order the author wrote them.
    Object(Vec<Member>),
}

/// One `"key": value` pair of an object.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub key: Spanned<String>,
    pub value: Spanned<Node>,
}

/// A parsed document and the text it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub root: Spanned<Node>,
    /// The input after BOM stripping. Spans index into this.
    pub source: String,
}

/// Why a text is not a JSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    /// Byte offset into the (BOM-stripped) source.
    pub offset: usize,
    /// 1-based, for messages.
    pub line: usize,
    /// 1-based, in characters, for messages.
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    UnexpectedEof,
    UnexpectedChar(char),
    InvalidNumber,
    InvalidEscape,
    LoneSurrogate,
    ControlCharInString,
    /// The key, and the offset of its *first* occurrence; the error's own
    /// offset is the second.
    DuplicateKey {
        key: String,
        first: usize,
    },
    TooDeep,
    TrailingContent,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}: ", self.line, self.column)?;
        match &self.kind {
            ParseErrorKind::UnexpectedEof => write!(f, "unexpected end of input"),
            ParseErrorKind::UnexpectedChar(c) => write!(f, "unexpected character {c:?}"),
            ParseErrorKind::InvalidNumber => write!(f, "invalid number"),
            ParseErrorKind::InvalidEscape => write!(f, "invalid escape sequence"),
            ParseErrorKind::LoneSurrogate => write!(f, "lone surrogate in \\u escape"),
            ParseErrorKind::ControlCharInString => {
                write!(f, "control character in string must be escaped")
            }
            ParseErrorKind::DuplicateKey { key, .. } => {
                write!(
                    f,
                    "duplicate key {key:?} — the runtime would keep only the last"
                )
            }
            ParseErrorKind::TooDeep => {
                write!(f, "nesting deeper than {MAX_DEPTH} levels")
            }
            ParseErrorKind::TrailingContent => write!(f, "trailing content after the document"),
        }
    }
}

impl std::error::Error for ParseError {}

impl Node {
    pub fn is_scalar(&self) -> bool {
        !matches!(self, Node::Array(_) | Node::Object(_))
    }

    pub fn as_object(&self) -> Option<&[Member]> {
        match self {
            Node::Object(members) => Some(members),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Spanned<Node>]> {
        match self {
            Node::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::String(s) => Some(s),
            _ => None,
        }
    }

    /// The member named `key`, if this is an object that has one.
    pub fn get(&self, key: &str) -> Option<&Spanned<Node>> {
        self.as_object()?
            .iter()
            .find(|m| m.key.node == key)
            .map(|m| &m.value)
    }

    /// Convert to the runtime's view.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Node::Null => serde_json::Value::Null,
            Node::Bool(b) => serde_json::Value::Bool(*b),
            // Validated at parse time to be a lexeme `serde_json::Number`
            // accepts, so this cannot fail on a node the parser produced.
            Node::Number(lexeme) => lexeme
                .parse::<serde_json::Number>()
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Node::String(s) => serde_json::Value::String(s.clone()),
            Node::Array(items) => {
                serde_json::Value::Array(items.iter().map(|i| i.node.to_value()).collect())
            }
            Node::Object(members) => serde_json::Value::Object(
                members
                    .iter()
                    .map(|m| (m.key.node.clone(), m.value.node.to_value()))
                    .collect(),
            ),
        }
    }
}

impl Document {
    /// Parse `text` strictly. See the module documentation for what "strict"
    /// adds to RFC 8259.
    pub fn parse(text: &str) -> Result<Document, ParseError> {
        let source = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut parser = Parser {
            bytes: source.as_bytes(),
            pos: 0,
        };
        let root = match parser.document() {
            Ok(root) => root,
            Err((kind, offset)) => {
                let (line, column) = line_col(source, offset);
                return Err(ParseError {
                    kind,
                    offset,
                    line,
                    column,
                });
            }
        };
        Ok(Document {
            root,
            source: source.to_string(),
        })
    }

    /// The runtime's view of the whole document.
    pub fn to_value(&self) -> serde_json::Value {
        self.root.node.to_value()
    }

    /// The span of the node at `path` — `tasks[1].function.input` — in the
    /// notation every `Finding` and `FieldError` already uses. `""` is the
    /// root. `None` when the path does not resolve.
    pub fn locate(&self, path: &str) -> Option<Span> {
        let mut current = &self.root;
        for segment in path_segments(path)? {
            current = match segment {
                Segment::Key(key) => current.node.get(&key)?,
                Segment::Index(i) => current.node.as_array()?.get(i)?,
            };
        }
        Some(current.span)
    }

    /// 1-based line and character column of a byte offset.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        line_col(&self.source, offset)
    }
}

/// 1-based line and character column of `offset` in `text`.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

enum Segment {
    Key(String),
    Index(usize),
}

/// `a.b[3].c` → `[Key(a), Key(b), Index(3), Key(c)]`. A key runs to the next
/// `.` or `[`; keys containing either cannot be addressed, which matches
/// every producer of these paths in the codebase.
fn path_segments(path: &str) -> Option<Vec<Segment>> {
    let mut out = Vec::new();
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('[') {
            let close = after.find(']')?;
            out.push(Segment::Index(after[..close].parse().ok()?));
            rest = &after[close + 1..];
            rest = rest.strip_prefix('.').unwrap_or(rest);
            continue;
        }
        let end = rest.find(['.', '[']).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        out.push(Segment::Key(rest[..end].to_string()));
        rest = &rest[end..];
        rest = rest.strip_prefix('.').unwrap_or(rest);
    }
    Some(out)
}

type Fail = (ParseErrorKind, usize);

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn document(&mut self) -> Result<Spanned<Node>, Fail> {
        self.skip_whitespace();
        let root = self.value(0)?;
        self.skip_whitespace();
        if self.pos < self.bytes.len() {
            return Err((ParseErrorKind::TrailingContent, self.pos));
        }
        Ok(root)
    }

    fn skip_whitespace(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// The character at `pos`, for an error message. The input is valid
    /// UTF-8, so decoding from a byte we stopped on is always a char start.
    fn char_at(&self, pos: usize) -> char {
        std::str::from_utf8(&self.bytes[pos..])
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or('\u{fffd}')
    }

    fn unexpected(&self) -> Fail {
        match self.peek() {
            None => (ParseErrorKind::UnexpectedEof, self.pos),
            Some(_) => (
                ParseErrorKind::UnexpectedChar(self.char_at(self.pos)),
                self.pos,
            ),
        }
    }

    fn value(&mut self, depth: usize) -> Result<Spanned<Node>, Fail> {
        let start = self.pos;
        let node = match self.peek() {
            None => return Err((ParseErrorKind::UnexpectedEof, self.pos)),
            Some(b'{') => self.object(depth)?,
            Some(b'[') => self.array(depth)?,
            Some(b'"') => Node::String(self.string()?),
            Some(b't') => self.literal(b"true", Node::Bool(true))?,
            Some(b'f') => self.literal(b"false", Node::Bool(false))?,
            Some(b'n') => self.literal(b"null", Node::Null)?,
            Some(b'-' | b'0'..=b'9') => self.number()?,
            Some(_) => return Err(self.unexpected()),
        };
        Ok(Spanned {
            node,
            span: Span {
                start,
                end: self.pos,
            },
        })
    }

    fn literal(&mut self, word: &[u8], node: Node) -> Result<Node, Fail> {
        if self.bytes[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(node)
        } else {
            // Point at the first byte that differs, so `tru` and `nul` say
            // where they went wrong rather than where they started.
            let mismatch = self.bytes[self.pos..]
                .iter()
                .zip(word)
                .position(|(a, b)| a != b)
                .unwrap_or(self.bytes.len() - self.pos);
            self.pos += mismatch;
            Err(self.unexpected())
        }
    }

    /// Enter a container: refuse past the depth `serde_json` refuses at.
    fn descend(&self, depth: usize) -> Result<usize, Fail> {
        if depth >= MAX_DEPTH {
            Err((ParseErrorKind::TooDeep, self.pos))
        } else {
            Ok(depth + 1)
        }
    }

    fn array(&mut self, depth: usize) -> Result<Node, Fail> {
        let depth = self.descend(depth)?;
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Node::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value(depth)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Node::Array(items));
                }
                _ => return Err(self.unexpected()),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Node, Fail> {
        let depth = self.descend(depth)?;
        self.pos += 1; // '{'
        let mut members: Vec<Member> = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Node::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.unexpected());
            }
            let key_start = self.pos;
            let key = self.string()?;
            let key = Spanned {
                node: key,
                span: Span {
                    start: key_start,
                    end: self.pos,
                },
            };
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.unexpected());
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.value(depth)?;
            members.push(Member { key, value });
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.unexpected()),
            }
        }
        check_duplicate_keys(&members)?;
        Ok(Node::Object(members))
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, then handed to
    /// `serde_json::Number` so an in-grammar lexeme it rejects — an exponent
    /// past f64's range — is refused here too.
    fn number(&mut self) -> Result<Node, Fail> {
        let start = self.pos;
        let fail = |pos| Err((ParseErrorKind::InvalidNumber, pos));
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return fail(self.pos),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return fail(self.pos);
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return fail(self.pos);
            }
            self.digits();
        }
        // Safe: the grammar above only ever consumed ASCII.
        let lexeme = std::str::from_utf8(&self.bytes[start..self.pos]).expect("ascii number");
        if lexeme.parse::<serde_json::Number>().is_err() {
            return fail(start);
        }
        Ok(Node::Number(lexeme.into()))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }

    /// A string starting at the opening quote. Returns it decoded.
    fn string(&mut self) -> Result<String, Fail> {
        self.pos += 1; // opening '"'
        let mut out = String::new();
        // Runs of ordinary bytes are copied in one go. The input is valid
        // UTF-8 and the run is only ever split at `"` or `\`, both ASCII, so
        // every slice is itself valid UTF-8.
        let mut run_start = self.pos;
        loop {
            let Some(b) = self.peek() else {
                return Err((ParseErrorKind::UnexpectedEof, self.pos));
            };
            match b {
                b'"' => {
                    out.push_str(self.run(run_start));
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    out.push_str(self.run(run_start));
                    self.escape(&mut out)?;
                    run_start = self.pos;
                }
                0x00..=0x1f => return Err((ParseErrorKind::ControlCharInString, self.pos)),
                _ => self.pos += 1,
            }
        }
    }

    fn run(&self, start: usize) -> &str {
        std::str::from_utf8(&self.bytes[start..self.pos]).expect("split at ascii")
    }

    /// An escape sequence starting at the backslash.
    fn escape(&mut self, out: &mut String) -> Result<(), Fail> {
        let at = self.pos;
        self.pos += 1;
        let Some(b) = self.peek() else {
            return Err((ParseErrorKind::UnexpectedEof, self.pos));
        };
        self.pos += 1;
        let c = match b {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let first = self.hex4(at)?;
                match first {
                    0xD800..=0xDBFF => {
                        // A high surrogate must be followed immediately by
                        // an escaped low surrogate; anything else is lone.
                        if !self.bytes[self.pos..].starts_with(b"\\u") {
                            return Err((ParseErrorKind::LoneSurrogate, at));
                        }
                        self.pos += 2;
                        let second = self.hex4(at)?;
                        if !(0xDC00..=0xDFFF).contains(&second) {
                            return Err((ParseErrorKind::LoneSurrogate, at));
                        }
                        let code = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                        char::from_u32(code).ok_or((ParseErrorKind::LoneSurrogate, at))?
                    }
                    0xDC00..=0xDFFF => return Err((ParseErrorKind::LoneSurrogate, at)),
                    _ => char::from_u32(first).ok_or((ParseErrorKind::InvalidEscape, at))?,
                }
            }
            _ => return Err((ParseErrorKind::InvalidEscape, at)),
        };
        out.push(c);
        Ok(())
    }

    fn hex4(&mut self, escape_start: usize) -> Result<u32, Fail> {
        let Some(hex) = self.bytes.get(self.pos..self.pos + 4) else {
            return Err((ParseErrorKind::UnexpectedEof, self.bytes.len()));
        };
        let mut value = 0u32;
        for &h in hex {
            let digit = (h as char)
                .to_digit(16)
                .ok_or((ParseErrorKind::InvalidEscape, escape_start))?;
            value = (value << 4) | digit;
        }
        self.pos += 4;
        Ok(value)
    }
}

/// Sort-and-compare rather than a set per object: objects are usually a
/// handful of keys, and this allocates nothing on the common path beyond one
/// index vector.
fn check_duplicate_keys(members: &[Member]) -> Result<(), Fail> {
    if members.len() < 2 {
        return Ok(());
    }
    let mut order: Vec<usize> = (0..members.len()).collect();
    // Stable, so among equal keys the earlier member stays first and the
    // report points at the second occurrence, not an arbitrary one.
    order.sort_by(|&a, &b| members[a].key.node.cmp(&members[b].key.node));
    for pair in order.windows(2) {
        let (first, second) = (&members[pair[0]], &members[pair[1]]);
        if first.key.node == second.key.node {
            return Err((
                ParseErrorKind::DuplicateKey {
                    key: first.key.node.clone(),
                    first: first.key.span.start,
                },
                second.key.span.start,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Document {
        Document::parse(text)
            .map_err(|e| format!("{text:?}: {e}"))
            .expect("test input parses")
    }

    fn kind(text: &str) -> ParseErrorKind {
        Document::parse(text).expect_err("should fail").kind
    }

    #[test]
    fn scalars_round_trip_through_to_value() {
        for text in [
            "null", "true", "false", "0", "-0", "1.0", "1e3", "-1.5E-7", "\"\"", "\"a\"",
        ] {
            assert_eq!(
                parse(text).to_value(),
                serde_json::from_str::<serde_json::Value>(text).expect("test input is valid"),
                "{text}"
            );
        }
    }

    #[test]
    fn numbers_keep_their_lexeme() {
        let doc = parse("[1.0, 1e3, -0, 123456789012345678901234567890]");
        let lexemes: Vec<&str> = doc
            .root
            .node
            .as_array()
            .expect("test input is valid")
            .iter()
            .map(|n| match &n.node {
                Node::Number(l) => &**l,
                other => unreachable!("only numbers in this fixture: {other:?}"),
            })
            .collect();
        assert_eq!(
            lexemes,
            ["1.0", "1e3", "-0", "123456789012345678901234567890"]
        );
    }

    #[test]
    fn keys_keep_author_order_and_spans_index_the_source() {
        let text = r#"{"z": 1, "a": [true, {"k": "v"}]}"#;
        let doc = parse(text);
        let keys: Vec<&str> = doc
            .root
            .node
            .as_object()
            .expect("test input is valid")
            .iter()
            .map(|m| m.key.node.as_str())
            .collect();
        assert_eq!(keys, ["z", "a"]);
        let span = doc.locate("a[1].k").expect("test input is valid");
        assert_eq!(&text[span.start..span.end], "\"v\"");
        let span = doc.locate("a[1]").expect("test input is valid");
        assert_eq!(&text[span.start..span.end], r#"{"k": "v"}"#);
        assert_eq!(doc.locate("").expect("test input is valid"), doc.root.span);
        assert!(doc.locate("a[9]").is_none());
        assert!(doc.locate("nope").is_none());
    }

    #[test]
    fn escapes_decode_and_surrogates_combine() {
        let doc = parse(r#""a\"b\\c\/d\n\t\u00e9\ud83d\ude00""#);
        assert_eq!(
            doc.root.node.as_str().expect("test input is valid"),
            "a\"b\\c/d\n\té😀"
        );
    }

    #[test]
    fn strictness_matches_serde_json() {
        // Each of these is refused by serde_json; this parser must agree,
        // and name a specific reason.
        let cases = [
            ("", ParseErrorKind::UnexpectedEof),
            ("[1,]", ParseErrorKind::UnexpectedChar(']')),
            ("{\"a\":1,}", ParseErrorKind::UnexpectedChar('}')),
            ("[01]", ParseErrorKind::UnexpectedChar('1')),
            ("[+1]", ParseErrorKind::UnexpectedChar('+')),
            ("[1.]", ParseErrorKind::InvalidNumber),
            ("[.5]", ParseErrorKind::UnexpectedChar('.')),
            ("[1e]", ParseErrorKind::InvalidNumber),
            ("[1e400]", ParseErrorKind::InvalidNumber),
            ("NaN", ParseErrorKind::UnexpectedChar('N')),
            ("'a'", ParseErrorKind::UnexpectedChar('\'')),
            ("\"a\nb\"", ParseErrorKind::ControlCharInString),
            ("\"\\x\"", ParseErrorKind::InvalidEscape),
            ("\"\\ud83d\"", ParseErrorKind::LoneSurrogate),
            ("\"\\ude00\"", ParseErrorKind::LoneSurrogate),
            ("\"\\ud83dx\"", ParseErrorKind::LoneSurrogate),
            ("tru", ParseErrorKind::UnexpectedEof),
            ("nul", ParseErrorKind::UnexpectedEof),
            ("truth", ParseErrorKind::UnexpectedChar('t')),
            ("{} x", ParseErrorKind::TrailingContent),
            ("// c\n{}", ParseErrorKind::UnexpectedChar('/')),
            ("{a: 1}", ParseErrorKind::UnexpectedChar('a')),
            ("\"open", ParseErrorKind::UnexpectedEof),
        ];
        for (text, expected) in cases {
            assert!(
                serde_json::from_str::<serde_json::Value>(text).is_err(),
                "serde_json accepts {text:?}"
            );
            assert_eq!(kind(text), expected, "{text:?}");
        }
    }

    #[test]
    fn duplicate_keys_are_refused_naming_both_offsets() {
        let err = Document::parse(r#"{"a": 1, "b": 2, "a": 3}"#).expect_err("must be refused");
        assert_eq!(
            err.kind,
            ParseErrorKind::DuplicateKey {
                key: "a".into(),
                first: 1
            }
        );
        assert_eq!(err.offset, 17);
        assert_eq!((err.line, err.column), (1, 18));
        // serde_json takes the last silently — the difference this exists for.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(r#"{"a": 1, "a": 3}"#)
                .expect("test input is valid")["a"],
            3
        );
    }

    #[test]
    fn depth_limit_is_serde_jsons() {
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        let too_deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(serde_json::from_str::<serde_json::Value>(&ok).is_ok());
        assert!(Document::parse(&ok).is_ok());
        assert!(serde_json::from_str::<serde_json::Value>(&too_deep).is_err());
        assert_eq!(kind(&too_deep), ParseErrorKind::TooDeep);
        // Objects count too.
        let objects = "{\"a\":".repeat(MAX_DEPTH + 1) + "1" + &"}".repeat(MAX_DEPTH + 1);
        assert_eq!(kind(&objects), ParseErrorKind::TooDeep);
    }

    #[test]
    fn a_pathological_document_fails_fast_rather_than_overflowing() {
        let text = "[".repeat(100_000);
        assert_eq!(kind(&text), ParseErrorKind::TooDeep);
    }

    #[test]
    fn bom_is_stripped_and_offsets_follow_the_stripped_text() {
        let doc = parse("\u{feff}{\"a\": 1}");
        assert_eq!(doc.source, "{\"a\": 1}");
        assert_eq!(doc.locate("a").expect("test input is valid").start, 6);
    }

    #[test]
    fn line_and_column_count_characters_across_crlf() {
        let err =
            Document::parse("{\r\n  \"é\": 1,\r\n  \"é\": 2\r\n}").expect_err("must be refused");
        assert_eq!((err.line, err.column), (3, 3));
        let doc = parse("[\n  1,\n  \"é\", 2\n]");
        assert_eq!(
            doc.line_col(doc.locate("[2]").expect("test input is valid").start),
            (3, 8)
        );
    }

    #[test]
    fn whitespace_is_only_the_four_json_kinds() {
        assert!(Document::parse(" \t\r\n[]\n").is_ok());
        assert_eq!(kind("\u{a0}[]"), ParseErrorKind::UnexpectedChar('\u{a0}'));
    }
}
