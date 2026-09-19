//! The one SQL lexer: what `db_read`'s read-only check, `db_write`'s
//! leading keyword, the `$sql` authoring pass and the SQL clippy rules all
//! read a statement with.
//!
//! It knows exactly as much SQL as those callers need: where comments,
//! quoted strings and identifiers, dollar-quoted bodies and bind
//! placeholders begin and end — never the grammar. Everything it skips is
//! kept as a token with its byte offset, so a caller can re-emit the text
//! (`normalize`) or point back into it (`line_col`).
//!
//! Two modes. [`tokens`] is **strict**: it refuses what it cannot lex the
//! same way on every backend — an unterminated run, a backslash before the
//! quote that would close a plain string (PostgreSQL ends the string there,
//! MySQL escapes the quote), and `--` glued to the next character
//! (PostgreSQL and SQLite start a comment, MySQL reads two minus signs). An
//! authoring tool must not guess between two readings of one statement.
//! [`tokens_lossy`] never refuses: an unterminated run extends to the end of
//! the input and the ambiguities take the PostgreSQL reading, which is what
//! the run-time check has always done.
//!
//! MySQL's `#` comments are not recognised: `#` is an operator in
//! PostgreSQL.
//!
//! A leaf: it names nothing in the crate.

/// One lexical token, borrowing its text from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokenKind,
    pub text: &'a str,
    /// Byte offset of `text` in the input.
    pub start: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    /// `-- …` up to, not including, the newline.
    LineComment,
    /// `/* … */`, nested. `hint` when it opens `/*!` (MySQL) or `/*+`
    /// (optimizer hints) — a comment the database reads.
    BlockComment {
        hint: bool,
    },
    /// `'…'` with `''` doubling. `escapes` for PostgreSQL's `E'…'`, where a
    /// backslash escapes the next character.
    SingleQuoted {
        escapes: bool,
    },
    /// `"…"` with `""` doubling.
    DoubleQuoted,
    /// `` `…` `` with doubled backticks, as MySQL writes them.
    Backtick,
    /// `$tag$ … $tag$` (or `$$ … $$`).
    DollarQuoted,
    Placeholder(Placeholder),
    /// Letters, digits, `_` and — after the first character — `$`, which
    /// PostgreSQL reads as part of an identifier (`a$b$`).
    Word,
    Open,
    Close,
    Semicolon,
    /// Any other single character.
    Other,
}

/// A bind placeholder outside every quoted form and comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placeholder {
    /// `$1`.
    Numbered(u32),
    /// A bare `?` — not `?|`, `?&` or `??` (PostgreSQL's jsonb operators,
    /// JDBC's escape).
    Positional,
}

/// Why the strict lexer refused a statement, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexError {
    pub kind: LexErrorKind,
    /// Byte offset in the input where the offending run begins.
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexErrorKind {
    UnterminatedString,
    UnterminatedIdentifier,
    UnterminatedComment,
    UnterminatedDollarQuote,
    AmbiguousBackslashQuote,
    AmbiguousDashDash,
    /// A `BEGIN ATOMIC … END` body, whose inner `;` do not end the
    /// statement — only [`statements`] reports it.
    AtomicBody,
}

impl std::fmt::Display for LexErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnterminatedString => "a quoted string is never closed",
            Self::UnterminatedIdentifier => "a quoted identifier is never closed",
            Self::UnterminatedComment => "a /* comment */ is never closed",
            Self::UnterminatedDollarQuote => "a $tag$ dollar-quoted body is never closed",
            Self::AmbiguousBackslashQuote => {
                "a backslash before a closing quote is read differently by PostgreSQL and \
                 MySQL — write '' for a quote, or E'…' on PostgreSQL"
            }
            Self::AmbiguousDashDash => {
                "'--' followed by a character is a comment on PostgreSQL and SQLite but two \
                 minus signs on MySQL — write '-- ' (with a space) for a comment, or '- -' for a \
                 double negation"
            }
            Self::AtomicBody => "a BEGIN ATOMIC body cannot be split into statements by the lexer",
        })
    }
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at byte {})", self.kind, self.offset)
    }
}

/// Strict lexing: what an authoring tool uses.
///
/// # Errors
///
/// An unterminated run, or one of the two dialect ambiguities.
pub fn tokens(sql: &str) -> Result<Vec<Token<'_>>, LexError> {
    let mut lexer = Lexer::new(sql, true);
    lexer.run()?;
    Ok(lexer.out)
}

/// Lossy lexing: an unterminated run extends to the end of the input and
/// the ambiguities take the PostgreSQL reading. Never refuses.
pub fn tokens_lossy(sql: &str) -> Vec<Token<'_>> {
    let mut lexer = Lexer::new(sql, false);
    // A lossy lexer has no error path.
    let _ = lexer.run();
    lexer.out
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    strict: bool,
    out: Vec<Token<'a>>,
    /// A lossy run that did not end where it should have — an unterminated
    /// quote or comment.
    unterminated: bool,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, strict: bool) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            strict,
            out: Vec::new(),
            unterminated: false,
        }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn char_at(&self, at: usize) -> Option<char> {
        self.src.get(at..)?.chars().next()
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.out.push(Token {
            kind,
            text: &self.src[start..self.pos],
            start,
        });
    }

    fn fail(&mut self, kind: LexErrorKind, offset: usize) -> Result<(), LexError> {
        if self.strict {
            return Err(LexError { kind, offset });
        }
        self.unterminated |= !matches!(
            kind,
            LexErrorKind::AmbiguousBackslashQuote | LexErrorKind::AmbiguousDashDash
        );
        Ok(())
    }

    /// Whether the character just before `at` continues a word — which
    /// makes a `$` there part of the word rather than a placeholder or a
    /// dollar quote.
    fn after_word_char(&self, at: usize) -> bool {
        self.src[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
    }

    fn run(&mut self) -> Result<(), LexError> {
        while self.pos < self.bytes.len() {
            let start = self.pos;
            let Some(c) = self.char_at(start) else {
                break;
            };
            match c {
                c if c.is_whitespace() => {
                    while self.char_at(self.pos).is_some_and(char::is_whitespace) {
                        self.pos += self.char_at(self.pos).map_or(1, char::len_utf8);
                    }
                    self.push(TokenKind::Whitespace, start);
                }
                '-' if self.peek(1) == Some(b'-') => {
                    if let Some(next) = self.char_at(start + 2)
                        && !next.is_whitespace()
                    {
                        self.fail(LexErrorKind::AmbiguousDashDash, start)?;
                    }
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                    self.push(TokenKind::LineComment, start);
                }
                '/' if self.peek(1) == Some(b'*') => {
                    let hint = matches!(self.peek(2), Some(b'!' | b'+'));
                    self.pos += 2;
                    let mut depth = 1usize;
                    while self.pos < self.bytes.len() && depth > 0 {
                        if self.bytes[self.pos] == b'/' && self.peek(1) == Some(b'*') {
                            depth += 1;
                            self.pos += 2;
                        } else if self.bytes[self.pos] == b'*' && self.peek(1) == Some(b'/') {
                            depth -= 1;
                            self.pos += 2;
                        } else {
                            self.pos += 1;
                        }
                    }
                    if depth > 0 {
                        self.fail(LexErrorKind::UnterminatedComment, start)?;
                    }
                    self.push(TokenKind::BlockComment { hint }, start);
                }
                '\'' => self.single_quoted(start, false)?,
                'E' | 'e' if self.peek(1) == Some(b'\'') && !self.after_word_char(start) => {
                    self.pos += 1;
                    self.single_quoted(start, true)?;
                }
                '"' => self.doubled(start, b'"', TokenKind::DoubleQuoted)?,
                '`' => self.doubled(start, b'`', TokenKind::Backtick)?,
                '$' if !self.after_word_char(start) => self.dollar(start)?,
                '?' => {
                    if matches!(self.peek(1), Some(b'|' | b'&' | b'?')) {
                        self.pos += 1;
                        self.push(TokenKind::Other, start);
                        let second = self.pos;
                        self.pos += 1;
                        self.push(TokenKind::Other, second);
                    } else {
                        self.pos += 1;
                        self.push(TokenKind::Placeholder(Placeholder::Positional), start);
                    }
                }
                '(' => {
                    self.pos += 1;
                    self.push(TokenKind::Open, start);
                }
                ')' => {
                    self.pos += 1;
                    self.push(TokenKind::Close, start);
                }
                ';' => {
                    self.pos += 1;
                    self.push(TokenKind::Semicolon, start);
                }
                c if c.is_alphanumeric() || c == '_' => {
                    while self
                        .char_at(self.pos)
                        .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
                    {
                        self.pos += self.char_at(self.pos).map_or(1, char::len_utf8);
                    }
                    self.push(TokenKind::Word, start);
                }
                other => {
                    self.pos += other.len_utf8();
                    self.push(TokenKind::Other, start);
                }
            }
        }
        Ok(())
    }

    /// A `'…'` string opening at `self.pos` (the quote), with `''` doubling;
    /// `escapes` for `E'…'`, where `start` is the `E`.
    fn single_quoted(&mut self, start: usize, escapes: bool) -> Result<(), LexError> {
        self.pos += 1;
        loop {
            let Some(b) = self.bytes.get(self.pos).copied() else {
                self.fail(LexErrorKind::UnterminatedString, start)?;
                break;
            };
            match b {
                b'\\' if escapes => self.pos += 2,
                b'\'' => {
                    // An odd run of backslashes before this quote is where
                    // PostgreSQL and MySQL part ways: one ends the string
                    // (or reads `''`), the other escapes the quote.
                    let backslashes = self.bytes[..self.pos]
                        .iter()
                        .rev()
                        .take_while(|&&c| c == b'\\')
                        .count();
                    if !escapes && backslashes % 2 == 1 {
                        self.fail(LexErrorKind::AmbiguousBackslashQuote, self.pos - 1)?;
                    }
                    if self.peek(1) == Some(b'\'') {
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        break;
                    }
                }
                _ => self.pos += 1,
            }
        }
        self.pos = self.pos.min(self.bytes.len());
        self.push(TokenKind::SingleQuoted { escapes }, start);
        Ok(())
    }

    /// A run delimited by `quote` with the quote doubled to escape it.
    fn doubled(&mut self, start: usize, quote: u8, kind: TokenKind) -> Result<(), LexError> {
        self.pos += 1;
        loop {
            match self.bytes.get(self.pos).copied() {
                None => {
                    self.fail(LexErrorKind::UnterminatedIdentifier, start)?;
                    break;
                }
                Some(b) if b == quote => {
                    if self.peek(1) == Some(quote) {
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        break;
                    }
                }
                Some(_) => self.pos += 1,
            }
        }
        self.push(kind, start);
        Ok(())
    }

    /// A `$` not continuing a word: a `$n` placeholder, a dollar quote, or
    /// a lone `$`.
    fn dollar(&mut self, start: usize) -> Result<(), LexError> {
        let digits = self.bytes[start + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits > 0 {
            self.pos = start + 1 + digits;
            match self.src[start + 1..self.pos].parse::<u32>() {
                Ok(n) => self.push(TokenKind::Placeholder(Placeholder::Numbered(n)), start),
                Err(_) => self.push(TokenKind::Other, start),
            }
            return Ok(());
        }
        // A tag: letters, `_`, then digits too; closed by another `$`.
        let tag_len = self.src[start + 1..]
            .char_indices()
            .take_while(|(i, c)| c.is_alphabetic() || *c == '_' || (*i > 0 && c.is_ascii_digit()))
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);
        if self.bytes.get(start + 1 + tag_len) != Some(&b'$') {
            self.pos = start + 1;
            self.push(TokenKind::Other, start);
            return Ok(());
        }
        let open = &self.src[start..start + tag_len + 2];
        self.pos = start + open.len();
        match self.src[self.pos..].find(open) {
            Some(at) => self.pos += at + open.len(),
            None => {
                self.fail(LexErrorKind::UnterminatedDollarQuote, start)?;
                self.pos = self.bytes.len();
            }
        }
        self.push(TokenKind::DollarQuoted, start);
        Ok(())
    }
}

// ============================================================
// Normal form
// ============================================================

/// A statement in `$sql`'s normal form, with the map back to its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub text: String,
    /// The runs of `text` copied verbatim from the source.
    pub segments: Vec<Segment>,
}

/// One verbatim run: `len` bytes at `out` in the normal form came from `src`
/// in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub out: usize,
    pub src: usize,
    pub len: usize,
}

impl Normalized {
    /// The source byte a normal-form byte came from, when it was copied
    /// rather than inserted (the single spaces are inserted).
    pub fn source_offset(&self, out_offset: usize) -> Option<usize> {
        self.segments
            .iter()
            .find(|s| s.out <= out_offset && out_offset < s.out + s.len)
            .map(|s| s.src + (out_offset - s.out))
    }
}

/// `$sql`'s normal form: every run of whitespace and comments becomes one
/// space (none at either end), everything else — strings, quoted
/// identifiers, dollar bodies, optimizer hints — is copied byte for byte,
/// and one trailing `;` is dropped. A leading UTF-8 byte-order mark is
/// ignored. Nothing is case-folded and an interior `;` is kept.
///
/// So a comment or an indentation change does not change the text, and a
/// comment is always a separator (`SELECT/**/1` → `SELECT 1`), as SQL
/// defines it.
///
/// # Errors
///
/// What the strict lexer refuses.
pub fn normalize(sql: &str) -> Result<Normalized, LexError> {
    let bom = if sql.starts_with('\u{feff}') { 3 } else { 0 };
    let body = &sql[bom..];
    let all = tokens(body).map_err(|e| LexError {
        offset: e.offset + bom,
        ..e
    })?;
    let separator = |t: &Token<'_>| {
        matches!(
            t.kind,
            TokenKind::Whitespace
                | TokenKind::LineComment
                | TokenKind::BlockComment { hint: false }
        )
    };
    let last_significant = all.iter().rposition(|t| !separator(t));
    let mut text = String::with_capacity(body.len());
    let mut segments = Vec::new();
    let mut pending = false;
    for (i, token) in all.iter().enumerate() {
        if separator(token) {
            pending = true;
            continue;
        }
        if Some(i) == last_significant && token.kind == TokenKind::Semicolon {
            continue;
        }
        if pending && !text.is_empty() {
            text.push(' ');
        }
        pending = false;
        segments.push(Segment {
            out: text.len(),
            src: token.start + bom,
            len: token.text.len(),
        });
        text.push_str(token.text);
    }
    Ok(Normalized { text, segments })
}

/// 1-based line and column of byte `offset` in `text`; the column counts
/// characters, as an editor shows it.
pub fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[line_start..].chars().count() + 1)
}

// ============================================================
// Placeholders and statements
// ============================================================

/// The bind placeholders a statement uses, outside every quoted form and
/// comment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placeholders {
    /// Every `$n` occurrence (n ≥ 1) in source order, duplicates kept.
    pub numbered: Vec<u32>,
    /// Bare `?` occurrences.
    pub positional: usize,
    /// A `::` cast or a `$tag$…$tag$` body was seen — syntax only
    /// PostgreSQL has, and so a proof of the backend.
    pub postgres_only_syntax: bool,
    /// `false` when the reading could differ by backend or the input did
    /// not lex cleanly: an `E'…'` string, a backslash inside `'…'`, or an
    /// unterminated run. A caller that needs certainty stays silent.
    pub certain: bool,
}

impl Placeholders {
    /// The highest `$n`.
    pub fn max_numbered(&self) -> Option<u32> {
        self.numbered.iter().copied().max()
    }

    /// The numbers below the highest that no `$n` uses.
    pub fn missing_numbered(&self) -> Vec<u32> {
        let Some(max) = self.max_numbered() else {
            return Vec::new();
        };
        (1..=max).filter(|n| !self.numbered.contains(n)).collect()
    }
}

/// The bind placeholders of `sql`, read with the lossy lexer.
pub fn placeholders(sql: &str) -> Placeholders {
    let mut lexer = Lexer::new(sql, false);
    let _ = lexer.run();
    let mut out = Placeholders {
        certain: !lexer.unterminated,
        ..Placeholders::default()
    };
    let all = &lexer.out;
    for (i, token) in all.iter().enumerate() {
        match token.kind {
            TokenKind::Placeholder(Placeholder::Numbered(n)) if n >= 1 => out.numbered.push(n),
            TokenKind::Placeholder(Placeholder::Positional) => out.positional += 1,
            TokenKind::DollarQuoted => out.postgres_only_syntax = true,
            TokenKind::SingleQuoted { escapes } => {
                if escapes || token.text.contains('\\') {
                    out.certain = false;
                }
            }
            TokenKind::Other
                if token.text == ":"
                    && all
                        .get(i + 1)
                        .is_some_and(|n| n.text == ":" && n.start == token.start + 1) =>
            {
                out.postgres_only_syntax = true;
            }
            _ => {}
        }
    }
    out
}

/// The top-level statements of `sql`, split at `;` outside quotes, comments
/// and dollar bodies, each trimmed; empty ones are dropped.
///
/// # Errors
///
/// What the strict lexer refuses, and a `BEGIN ATOMIC` body — its inner `;`
/// do not end the statement, and following its nesting is grammar, not
/// lexing.
pub fn statements(sql: &str) -> Result<Vec<&str>, LexError> {
    let all = tokens(sql)?;
    let significant: Vec<&Token<'_>> = all
        .iter()
        .filter(|t| {
            !matches!(
                t.kind,
                TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment { .. }
            )
        })
        .collect();
    for pair in significant.windows(2) {
        if pair[0].kind == TokenKind::Word
            && pair[0].text.eq_ignore_ascii_case("BEGIN")
            && pair[1].kind == TokenKind::Word
            && pair[1].text.eq_ignore_ascii_case("ATOMIC")
        {
            return Err(LexError {
                kind: LexErrorKind::AtomicBody,
                offset: pair[0].start,
            });
        }
    }
    let mut out = Vec::new();
    let mut from = 0;
    for token in all.iter().filter(|t| t.kind == TokenKind::Semicolon) {
        let piece = sql[from..token.start].trim();
        if !piece.is_empty() {
            out.push(piece);
        }
        from = token.start + 1;
    }
    let tail = sql[from..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    Ok(out
        .into_iter()
        .filter(|s| {
            tokens_lossy(s).iter().any(|t| {
                !matches!(
                    t.kind,
                    TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment { .. }
                )
            })
        })
        .collect())
}

// ============================================================
// Statement shape
// ============================================================

/// The statement kinds that return rows without modifying them.
///
/// Deliberately short. `EXPLAIN` is **not** here: `EXPLAIN ANALYZE DELETE …`
/// executes the delete on PostgreSQL. Neither is `PRAGMA`, which writes on
/// SQLite (`PRAGMA journal_mode = WAL`). A statement that needs to write
/// belongs in `db_write`, which has its own `raw_write` gate.
pub const READ_STATEMENTS: [&str; 4] = ["SELECT", "WITH", "VALUES", "TABLE"];

/// The keywords that make a CTE data-modifying.
pub const MODIFYING_STATEMENTS: [&str; 4] = ["INSERT", "UPDATE", "DELETE", "MERGE"];

/// The two token shapes the shape checks read — a word (upper-cased) and an
/// opening parenthesis, which is what separates a data-modifying CTE from a
/// column alias (`AS (INSERT …` versus `AS total`). Everything else — quoted
/// text, comments, punctuation — is skipped, which is what keeps the check
/// from reading data as syntax: `WHERE note = 'delete me'` is a read.
#[derive(Debug, PartialEq, Eq)]
enum Shape {
    Word(String),
    Open,
}

fn shape(sql: &str) -> Vec<Shape> {
    tokens_lossy(sql)
        .into_iter()
        .filter_map(|t| match t.kind {
            TokenKind::Word => Some(Shape::Word(t.text.to_uppercase())),
            TokenKind::Open => Some(Shape::Open),
            _ => None,
        })
        .collect()
}

/// The statement's leading keyword, upper-cased, with comments and quoted
/// text ignored — `None` for a statement with no keyword at all. A leading
/// `(` is ordinary — `(SELECT 1) UNION (SELECT 2)`.
pub fn leading_keyword(sql: &str) -> Option<String> {
    shape(sql).into_iter().find_map(|s| match s {
        Shape::Word(w) => Some(w),
        Shape::Open => None,
    })
}

/// Why a statement is not a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyViolation {
    /// No statement at all.
    Empty,
    /// It opens with `keyword`, which is not one of [`READ_STATEMENTS`].
    NotARead { keyword: String },
    /// It carries a data-modifying common table expression — `WITH moved AS
    /// (DELETE … RETURNING …) SELECT …`.
    ModifyingCte { keyword: String },
}

/// Whether `sql` is a read, and if not, why.
pub fn read_only_violation(sql: &str) -> Option<ReadOnlyViolation> {
    let shapes = shape(sql);
    let Some(first) = leading_keyword(sql) else {
        return Some(ReadOnlyViolation::Empty);
    };
    if !READ_STATEMENTS.contains(&first.as_str()) {
        return Some(ReadOnlyViolation::NotARead { keyword: first });
    }
    // A data-modifying CTE opens with `WITH` and writes. It is recognisable
    // by shape: `AS`, an optional `[NOT] MATERIALIZED`, `(`, then the
    // modifying keyword. A column alias (`AS total`) and an ordinary CTE
    // (`AS (SELECT …)`) both fail to match.
    for (n, token) in shapes.iter().enumerate() {
        if !matches!(token, Shape::Word(w) if w == "AS") {
            continue;
        }
        let mut j = n + 1;
        while matches!(shapes.get(j), Some(Shape::Word(w)) if w == "NOT" || w == "MATERIALIZED") {
            j += 1;
        }
        if shapes.get(j) != Some(&Shape::Open) {
            continue;
        }
        if let Some(Shape::Word(w)) = shapes.get(j + 1)
            && MODIFYING_STATEMENTS.contains(&w.as_str())
        {
            return Some(ReadOnlyViolation::ModifyingCte { keyword: w.clone() });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(sql: &str) -> String {
        normalize(sql).expect("normalizes").text
    }

    fn error(sql: &str) -> LexErrorKind {
        normalize(sql).expect_err("refused").kind
    }

    #[test]
    fn reads_and_writes_are_told_apart() {
        for sql in [
            "SELECT id FROM users WHERE id = $1",
            "  \n select 1",
            "-- a comment\nSELECT 1",
            "/* block */ SELECT 1",
            "(SELECT 1) UNION (SELECT 2)",
            "WITH recent AS (SELECT * FROM orders) SELECT * FROM recent",
            "VALUES (1), (2)",
            "TABLE users",
            "SELECT id FROM jobs ORDER BY id FOR UPDATE SKIP LOCKED",
            "SELECT id FROM notes WHERE body = 'delete from users'",
            "SELECT \"delete\" FROM t",
            "SELECT total AS deleted FROM t",
            "SELECT CAST(a AS text) FROM t",
            "SELECT 1 /* AS (DELETE */",
            "SELECT 'x AS (DELETE FROM t)' AS s",
            "SELECT $tag$ AS (DELETE FROM t) $tag$ AS s",
        ] {
            assert_eq!(read_only_violation(sql), None, "{sql}");
        }
        for sql in [
            "DELETE FROM t",
            "EXPLAIN ANALYZE DELETE FROM t",
            "PRAGMA journal_mode = WAL",
        ] {
            assert!(
                matches!(
                    read_only_violation(sql),
                    Some(ReadOnlyViolation::NotARead { .. })
                ),
                "{sql}"
            );
        }
        assert_eq!(
            read_only_violation(
                "with m as materialized (update t set a = 1 returning id) select 1"
            ),
            Some(ReadOnlyViolation::ModifyingCte {
                keyword: "UPDATE".to_string()
            })
        );
        assert_eq!(
            read_only_violation("  -- nothing\n"),
            Some(ReadOnlyViolation::Empty)
        );
    }

    #[test]
    fn a_string_literal_keeps_its_inner_whitespace() {
        assert_eq!(norm("SELECT   'a   b'\n  FROM t"), "SELECT 'a   b' FROM t");
    }

    #[test]
    fn a_double_dash_inside_a_quoted_identifier_is_not_a_comment() {
        assert_eq!(norm("SELECT \"a--b\" FROM t"), "SELECT \"a--b\" FROM t");
        assert_eq!(norm("SELECT 'a -- b' FROM t"), "SELECT 'a -- b' FROM t");
    }

    #[test]
    fn a_comment_marker_inside_a_dollar_quote_is_text() {
        let sql = "SELECT $body$ -- not a comment /* nor this */ $body$ AS s";
        assert_eq!(norm(sql), sql);
    }

    #[test]
    fn hints_survive_and_comments_do_not() {
        assert_eq!(
            norm("SELECT /*+ INDEX(t i) */ a /* note */ FROM /* a /* nested */ b */ t"),
            "SELECT /*+ INDEX(t i) */ a FROM t"
        );
        assert_eq!(
            norm("SELECT /*! STRAIGHT_JOIN */ 1"),
            "SELECT /*! STRAIGHT_JOIN */ 1"
        );
    }

    #[test]
    fn a_comment_is_a_separator() {
        assert_eq!(norm("SELECT/**/1"), "SELECT 1");
        assert_eq!(norm("SELECT 1-- trailing\n"), "SELECT 1");
    }

    #[test]
    fn one_trailing_semicolon_is_dropped_and_an_interior_one_is_kept() {
        assert_eq!(norm("SELECT 1;"), "SELECT 1");
        assert_eq!(norm("SELECT 1; -- done\n"), "SELECT 1");
        assert_eq!(norm("SELECT 1; SELECT 2;"), "SELECT 1; SELECT 2");
        assert_eq!(norm("SELECT 1;;"), "SELECT 1;");
    }

    #[test]
    fn a_bom_and_crlf_outside_quotes_vanish() {
        assert_eq!(
            norm("\u{feff}SELECT a,\r\n       b\r\nFROM t\r\n"),
            "SELECT a, b FROM t"
        );
        // Inside a literal a CRLF is data.
        assert_eq!(norm("SELECT 'a\r\nb'"), "SELECT 'a\r\nb'");
    }

    #[test]
    fn normalizing_is_idempotent_and_comment_edits_do_not_move_it() {
        for sql in [
            "SELECT a,\n  b -- the b\nFROM t WHERE x = $1;",
            "/* header */\nWITH r AS (SELECT 1) SELECT * FROM r",
            "SELECT $$ a  b $$, 'c  d', \"e  f\", `g``h`",
        ] {
            let once = norm(sql);
            assert_eq!(norm(&once), once, "{sql}");
        }
        assert_eq!(
            norm("SELECT a -- one comment\nFROM t"),
            norm("SELECT a /* another */ FROM t")
        );
    }

    #[test]
    fn each_refusal_names_its_offset() {
        assert_eq!(error("SELECT 'open"), LexErrorKind::UnterminatedString);
        assert_eq!(error("SELECT \"open"), LexErrorKind::UnterminatedIdentifier);
        assert_eq!(error("SELECT `open"), LexErrorKind::UnterminatedIdentifier);
        assert_eq!(error("SELECT 1 /* open"), LexErrorKind::UnterminatedComment);
        assert_eq!(
            error("SELECT $t$ open"),
            LexErrorKind::UnterminatedDollarQuote
        );
        assert_eq!(
            error("SELECT 'it\\'s'"),
            LexErrorKind::AmbiguousBackslashQuote
        );
        assert_eq!(error("SELECT 1 --x\n"), LexErrorKind::AmbiguousDashDash);
        let e = normalize("SELECT 1, 'open").expect_err("refused");
        assert_eq!(e.offset, 10);
        // Two backslashes are one escaped backslash on MySQL and two on
        // PostgreSQL, but the quote closes the string on both.
        assert_eq!(norm("SELECT 'a\\\\'"), "SELECT 'a\\\\'");
        // `E'…'` is PostgreSQL's own escape form, and unambiguous.
        assert_eq!(norm("SELECT E'it\\'s'"), "SELECT E'it\\'s'");
        // `-- ` and `--` at the end are ordinary comments.
        assert_eq!(norm("SELECT 1 --"), "SELECT 1");
    }

    #[test]
    fn segments_map_back_to_the_source_line() {
        let sql = "-- header\nSELECT a\n  FROM t\n WHERE b = $1";
        let n = normalize(sql).expect("normalizes");
        let at = n.text.find("WHERE").expect("where");
        let src = n.source_offset(at).expect("copied");
        assert_eq!(line_col(sql, src), (4, 2));
        // The inserted spaces map nowhere.
        let space = n.text.find(' ').expect("space");
        assert_eq!(n.source_offset(space), None);
    }

    #[test]
    fn placeholders_ignore_strings_comments_and_dollar_bodies() {
        let p = placeholders(
            "SELECT $1, '$2', \"$3\", /* $4 */ $$ $5 $$, $2 -- $6\n FROM t WHERE a = $1",
        );
        assert_eq!(p.numbered, vec![1, 2, 1]);
        assert_eq!(p.positional, 0);
        assert!(p.postgres_only_syntax, "a dollar body is PostgreSQL's");
        assert!(p.certain);
        assert_eq!(p.max_numbered(), Some(2));
        assert_eq!(placeholders("SELECT $1, $3").missing_numbered(), vec![2]);
    }

    #[test]
    fn a_dollar_inside_an_identifier_is_not_a_placeholder() {
        let p = placeholders("SELECT a$1, b$tag$ FROM t$x WHERE c = $2");
        assert_eq!(p.numbered, vec![2]);
        assert!(!p.postgres_only_syntax);
        assert_eq!(norm("SELECT a$b$ FROM t"), "SELECT a$b$ FROM t");
    }

    #[test]
    fn jsonb_question_operators_are_not_positional() {
        let p = placeholders(
            "SELECT * FROM t WHERE doc ?| array['a'] AND doc ?& b AND x = ? AND y ?? z",
        );
        assert_eq!(p.positional, 1);
        let cast = placeholders("SELECT a::text FROM t WHERE b = ?");
        assert!(cast.postgres_only_syntax);
        assert_eq!(cast.positional, 1);
    }

    #[test]
    fn uncertain_readings_are_flagged() {
        assert!(!placeholders("SELECT E'a\\'b' WHERE x = $1").certain);
        assert!(!placeholders("SELECT 'a\\b' WHERE x = $1").certain);
        assert!(!placeholders("SELECT 'open WHERE x = $1").certain);
        assert!(placeholders("SELECT 'a''b' WHERE x = $1").certain);
    }

    #[test]
    fn backticks_double() {
        assert_eq!(norm("SELECT `a``b` FROM t"), "SELECT `a``b` FROM t");
        assert_eq!(leading_keyword("`x``y` SELECT"), Some("SELECT".to_string()));
    }

    #[test]
    fn statements_split_outside_quotes_and_refuse_an_atomic_body() {
        assert_eq!(
            statements("CREATE TABLE a (x int);\n-- c\nINSERT INTO a VALUES (';');;")
                .expect("split"),
            vec!["CREATE TABLE a (x int)", "-- c\nINSERT INTO a VALUES (';')"]
        );
        assert_eq!(
            statements("CREATE FUNCTION f() BEGIN ATOMIC SELECT 1; END;")
                .expect_err("atomic")
                .kind,
            LexErrorKind::AtomicBody
        );
    }
}
