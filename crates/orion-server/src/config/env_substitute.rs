//! Env-var substitution for config text (TOML or JSON-as-string).
//!
//! Recognized syntax:
//! - `${VAR}` — required; errors if `VAR` is unset.
//! - `${VAR:-default}` — optional; uses `default` (which may be empty)
//!   when `VAR` is unset. A variable set to the empty string is inserted as
//!   empty: the default is for *unset* only.
//! - `${VAR:?message}` — required with a reason; errors, quoting `message`,
//!   when `VAR` is unset **or empty**.
//! - `$$` — escape for a literal `$`.
//!
//! A default or a message may itself contain placeholders —
//! `${A:-${B:-c}}` — up to [`MAX_NESTING`] levels, and is evaluated only
//! when it is needed, so the inner `B` is required only when `A` is unset.
//! Only `${` opens a level: a bare `{` in a default is text, so
//! `${A:-{"a":1}}` still yields `{"a":1}` (the default is `{"a":1`, and the
//! second `}` is a literal).
//!
//! The text is parsed first and evaluated second, and only the *input* is
//! ever parsed: the value of `${VAR}` is inserted verbatim and is not itself
//! scanned. That avoids any chance of an env-var injecting another `${...}`
//! lookup.
//!
//! [`Syntax::Toml`] additionally knows TOML's comments and string forms, so
//! nothing after a `#` that starts a comment is substituted: a comment that
//! mentions `${R2_ENDPOINT}` does not make that variable required at boot.

use crate::errors::OrionError;

/// How deep `${…}` may nest inside a default or a message.
pub const MAX_NESTING: usize = 8;

/// What kind of text is being substituted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// A TOML document: `#` comments and TOML's string forms are
    /// recognised, and nothing inside a comment is touched.
    Toml,
    /// Every byte is substitutable text — a JSON blob or a single value,
    /// where `#` is data (a URL fragment, a JSON string).
    Plain,
}

/// Substitute `${VAR}` / `${VAR:-default}` / `${VAR:?message}` in `input`
/// using the process environment. Every byte is substitutable text.
pub fn substitute(input: &str, source_label: &str) -> Result<String, OrionError> {
    substitute_with(input, source_label, Syntax::Plain, |k| {
        std::env::var(k).ok()
    })
}

/// [`substitute`] for a TOML document: placeholders inside comments are
/// left alone.
pub fn substitute_toml(input: &str, source_label: &str) -> Result<String, OrionError> {
    substitute_with(input, source_label, Syntax::Toml, |k| std::env::var(k).ok())
}

/// Every variable name `input` references through any placeholder form, at
/// any nesting depth.
///
/// The unknown-`ORION_*`-variable guard (C4d) needs these: substitution reads
/// arbitrary names out of the config file, so a file saying
/// `url = "${ORION_DB_URL}"` makes that variable one Orion genuinely reads,
/// even though no override is named after it. Nested names count too:
/// `${ORION_A:-${ORION_B}}` reads `ORION_B` whenever `ORION_A` is unset.
///
/// Read off the parsed text, so the `${…}` grammar lives in exactly one
/// place and nothing is evaluated — a missing required variable cannot fail
/// here; the real [`substitute`] call reports that. Text that does not parse
/// yields the names found before the error, and `substitute` reports it.
pub fn referenced_vars(input: &str) -> std::collections::BTreeSet<String> {
    referenced_vars_as(input, Syntax::Plain)
}

/// [`referenced_vars`] for a TOML document: a name mentioned only in a
/// comment is not referenced.
pub fn referenced_vars_toml(input: &str) -> std::collections::BTreeSet<String> {
    referenced_vars_as(input, Syntax::Toml)
}

fn referenced_vars_as(input: &str, syntax: Syntax) -> std::collections::BTreeSet<String> {
    let mut parser = Parser::new(input, syntax);
    let _ = parser.document();
    parser.names.iter().map(|n| (*n).to_string()).collect()
}

/// Substitute every placeholder in `input` using `lookup` as the variable
/// resolver. Used by tests to avoid touching the real env.
pub fn substitute_with<F>(
    input: &str,
    source_label: &str,
    syntax: Syntax,
    lookup: F,
) -> Result<String, OrionError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut parser = Parser::new(input, syntax);
    let pieces = parser
        .document()
        .map_err(|e| e.into_error(input, source_label))?;
    let mut out = String::with_capacity(input.len());
    eval(&pieces, &lookup, &mut out).map_err(|e| e.into_error(input, source_label))?;
    Ok(out)
}

// ============================================================
// The parsed form
// ============================================================

enum Piece<'a> {
    Text(&'a str),
    /// `$$`.
    Dollar,
    Ref(Placeholder<'a>),
}

struct Placeholder<'a> {
    name: &'a str,
    /// Byte offset of the `$`, for error positions.
    at: usize,
    op: Op<'a>,
}

enum Op<'a> {
    /// `${VAR}`
    Required,
    /// `${VAR:-default}`
    Default(Vec<Piece<'a>>),
    /// `${VAR:?message}`
    RequiredMsg(Vec<Piece<'a>>),
}

/// An error before it has a label and a position rendered: the byte offset
/// is turned into `line:col` only on this path.
enum Failure {
    Unterminated {
        at: usize,
    },
    TooDeep {
        at: usize,
    },
    InvalidName {
        at: usize,
        name: String,
    },
    Unsupported {
        at: usize,
        name: String,
        op: String,
    },
    Unset {
        at: usize,
        name: String,
    },
    Required {
        at: usize,
        name: String,
        message: String,
    },
}

impl Failure {
    fn into_error(self, input: &str, label: &str) -> OrionError {
        let pos = |at: usize| {
            let (line, col) = line_col(input, at);
            format!("{label}:{line}:{col}")
        };
        let message = match self {
            Failure::Unterminated { at } => {
                format!("Unterminated ${{…}} in {} — no matching '}}'", pos(at))
            }
            Failure::TooDeep { at } => {
                format!(
                    "${{…}} nested deeper than {MAX_NESTING} levels in {}",
                    pos(at)
                )
            }
            Failure::InvalidName { at, name } => format!(
                "Invalid env var name '{name}' in {} (allowed: [A-Za-z0-9_])",
                pos(at)
            ),
            Failure::Unsupported { at, name, op } => format!(
                "Unsupported substitution '${{{name}{op}…}}' in {} — the supported forms are \
                 ${{{name}}}, ${{{name}:-default}} and ${{{name}:?message}}",
                pos(at)
            ),
            Failure::Unset { at, name } => format!(
                "Required environment variable '{name}' is not set (referenced in {}). \
                 Set the variable or use '${{{name}:-default}}' to provide a fallback.",
                pos(at)
            ),
            Failure::Required { at, name, message } if message.is_empty() => {
                format!("{name} is required ({})", pos(at))
            }
            Failure::Required { at, name, message } => {
                format!("{name} is required: {message} ({})", pos(at))
            }
        };
        OrionError::Config { message }
    }
}

/// 1-based line and column of byte `at`; the column counts characters, not
/// bytes, so it matches what an editor shows.
fn line_col(input: &str, at: usize) -> (usize, usize) {
    let before = &input[..at.min(input.len())];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[line_start..].chars().count() + 1)
}

// ============================================================
// Parsing
// ============================================================

/// Where the top-level scan is in TOML's lexical grammar. Only the input's
/// own text moves it; a placeholder is opaque, so a default containing `"`
/// or `#` changes nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lex {
    Normal,
    Comment,
    Basic,
    MlBasic,
    Literal,
    MlLiteral,
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    syntax: Syntax,
    /// Every placeholder name, in order, as parsed — what
    /// [`referenced_vars`] returns, including when a later part fails.
    names: Vec<&'a str>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, syntax: Syntax) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            syntax,
            names: Vec::new(),
        }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        self.bytes[self.pos..].starts_with(s.as_bytes())
    }

    /// Advance over one character and return the text it spans.
    fn advance_char(&mut self) -> &'a str {
        let end = next_char_boundary(self.bytes, self.pos);
        let text = &self.input[self.pos..end];
        self.pos = end;
        text
    }

    /// The whole input: text, `$$` and placeholders, with TOML comments and
    /// string forms tracked when the syntax asks for it.
    fn document(&mut self) -> Result<Vec<Piece<'a>>, Failure> {
        let mut pieces = Vec::new();
        let mut lex = Lex::Normal;
        let mut text_start = self.pos;
        while self.pos < self.bytes.len() {
            if lex == Lex::Comment {
                // Nothing in a comment is parsed: no `${`, no `$$`.
                if self.bytes[self.pos] == b'\n' {
                    lex = Lex::Normal;
                }
                self.advance_char();
                continue;
            }
            if self.starts_with("$$") {
                push_text(&mut pieces, &self.input[text_start..self.pos]);
                pieces.push(Piece::Dollar);
                self.pos += 2;
                text_start = self.pos;
                continue;
            }
            if self.starts_with("${") {
                push_text(&mut pieces, &self.input[text_start..self.pos]);
                pieces.push(Piece::Ref(self.placeholder(1)?));
                text_start = self.pos;
                continue;
            }
            if self.syntax == Syntax::Toml {
                lex = self.toml_step(lex);
            } else {
                self.advance_char();
            }
        }
        push_text(&mut pieces, &self.input[text_start..self.pos]);
        Ok(pieces)
    }

    /// Consume the next token of TOML's lexical grammar outside a
    /// placeholder and return the state after it. A single-line string
    /// ends at a newline as well as at its quote, so one malformed line
    /// cannot swallow the rest of the file — the TOML parser then reports
    /// the real error with its own position.
    fn toml_step(&mut self, lex: Lex) -> Lex {
        let b = self.bytes[self.pos];
        match lex {
            Lex::Normal => match b {
                b'#' => {
                    self.pos += 1;
                    Lex::Comment
                }
                b'"' if self.starts_with("\"\"\"") => {
                    self.pos += 3;
                    Lex::MlBasic
                }
                b'"' => {
                    self.pos += 1;
                    Lex::Basic
                }
                b'\'' if self.starts_with("'''") => {
                    self.pos += 3;
                    Lex::MlLiteral
                }
                b'\'' => {
                    self.pos += 1;
                    Lex::Literal
                }
                _ => {
                    self.advance_char();
                    Lex::Normal
                }
            },
            Lex::Basic | Lex::Literal => {
                let quote = if lex == Lex::Basic { b'"' } else { b'\'' };
                if b == quote || b == b'\n' {
                    self.pos += 1;
                    Lex::Normal
                } else if lex == Lex::Basic
                    && b == b'\\'
                    && matches!(self.peek(1), Some(b'"' | b'\\'))
                {
                    self.pos += 2;
                    lex
                } else {
                    self.advance_char();
                    lex
                }
            }
            Lex::MlBasic | Lex::MlLiteral => {
                let quote = if lex == Lex::MlBasic { b'"' } else { b'\'' };
                if b == quote {
                    // A run of three to five quotes closes the string: up
                    // to two may belong to its content.
                    let run = self.bytes[self.pos..]
                        .iter()
                        .take_while(|&&c| c == quote)
                        .count();
                    self.pos += run;
                    if run >= 3 { Lex::Normal } else { lex }
                } else if lex == Lex::MlBasic
                    && b == b'\\'
                    && matches!(self.peek(1), Some(b'"' | b'\\'))
                {
                    self.pos += 2;
                    lex
                } else {
                    self.advance_char();
                    lex
                }
            }
            Lex::Comment => unreachable!("comments are consumed by the caller"),
        }
    }

    /// A placeholder starting at `self.pos` (on its `$`), `depth` levels
    /// deep. Leaves `self.pos` after its closing `}`.
    fn placeholder(&mut self, depth: usize) -> Result<Placeholder<'a>, Failure> {
        let at = self.pos;
        if depth > MAX_NESTING {
            return Err(Failure::TooDeep { at });
        }
        self.pos += 2;
        let name_start = self.pos;
        while self
            .peek(0)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.pos += 1;
        }
        let name = &self.input[name_start..self.pos];
        let op = match (self.peek(0), self.peek(1)) {
            (Some(b'}'), _) if !name.is_empty() => {
                self.pos += 1;
                self.names.push(name);
                return Ok(Placeholder {
                    name,
                    at,
                    op: Op::Required,
                });
            }
            (Some(b':'), Some(b'-')) if !name.is_empty() => Op::Default(Vec::new()),
            (Some(b':'), Some(b'?')) if !name.is_empty() => Op::RequiredMsg(Vec::new()),
            (Some(c), next)
                if !name.is_empty() && matches!(c, b':' | b'-' | b'=' | b'+' | b'?') =>
            {
                let mut op = (c as char).to_string();
                if c == b':'
                    && let Some(n) = next.filter(|n| n.is_ascii_punctuation() && *n != b'}')
                {
                    op.push(n as char);
                }
                return Err(Failure::Unsupported {
                    at,
                    name: name.to_string(),
                    op,
                });
            }
            (None, _) => return Err(Failure::Unterminated { at }),
            _ => {
                // Name the whole would-be name: the text up to the first
                // `:-`, `:?` or `}`.
                let rest = &self.input[name_start..];
                let end = ["}", ":-", ":?"]
                    .iter()
                    .filter_map(|t| rest.find(t))
                    .min()
                    .ok_or(Failure::Unterminated { at })?;
                return Err(Failure::InvalidName {
                    at,
                    name: rest[..end].to_string(),
                });
            }
        };
        self.names.push(name);
        self.pos += 2;
        let body = self.pieces_until_close(at, depth)?;
        Ok(Placeholder {
            name,
            at,
            op: match op {
                Op::Default(_) => Op::Default(body),
                _ => Op::RequiredMsg(body),
            },
        })
    }

    /// The pieces of a default or a message, up to and over the `}` that
    /// closes the placeholder opened at `at`.
    fn pieces_until_close(&mut self, at: usize, depth: usize) -> Result<Vec<Piece<'a>>, Failure> {
        let mut pieces = Vec::new();
        let mut text_start = self.pos;
        loop {
            match self.peek(0) {
                None => return Err(Failure::Unterminated { at }),
                Some(b'}') => {
                    push_text(&mut pieces, &self.input[text_start..self.pos]);
                    self.pos += 1;
                    return Ok(pieces);
                }
                Some(b'$') if self.peek(1) == Some(b'$') => {
                    push_text(&mut pieces, &self.input[text_start..self.pos]);
                    pieces.push(Piece::Dollar);
                    self.pos += 2;
                    text_start = self.pos;
                }
                Some(b'$') if self.peek(1) == Some(b'{') => {
                    push_text(&mut pieces, &self.input[text_start..self.pos]);
                    pieces.push(Piece::Ref(self.placeholder(depth + 1)?));
                    text_start = self.pos;
                }
                Some(_) => {
                    self.advance_char();
                }
            }
        }
    }
}

fn push_text<'a>(pieces: &mut Vec<Piece<'a>>, text: &'a str) {
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
}

// ============================================================
// Evaluation
// ============================================================

/// Evaluate `pieces` into `out`. A default or a message is evaluated only
/// when it is used, and a looked-up value is pushed verbatim — never parsed.
fn eval<F>(pieces: &[Piece<'_>], lookup: &F, out: &mut String) -> Result<(), Failure>
where
    F: Fn(&str) -> Option<String>,
{
    for piece in pieces {
        match piece {
            Piece::Text(t) => out.push_str(t),
            Piece::Dollar => out.push('$'),
            Piece::Ref(p) => match (&p.op, lookup(p.name)) {
                (Op::RequiredMsg(_), Some(v)) if !v.is_empty() => out.push_str(&v),
                (Op::Required | Op::Default(_), Some(v)) => out.push_str(&v),
                (Op::Required, None) => {
                    return Err(Failure::Unset {
                        at: p.at,
                        name: p.name.to_string(),
                    });
                }
                (Op::Default(d), None) => eval(d, lookup, out)?,
                (Op::RequiredMsg(m), _) => {
                    let mut message = String::new();
                    eval(m, lookup, &mut message)?;
                    return Err(Failure::Required {
                        at: p.at,
                        name: p.name.to_string(),
                        message: message.trim().to_string(),
                    });
                }
            },
        }
    }
    Ok(())
}

fn next_char_boundary(bytes: &[u8], i: usize) -> usize {
    // UTF-8 leading-byte classification. Continuation bytes (0x80..0xC0)
    // shouldn't appear as a char start in valid UTF-8 input — treat them
    // as 1-byte advances to keep the loop progressing safely.
    let b = bytes[i];
    let len = if b < 0xC0 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    };
    (i + len).min(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn plain(input: &str, pairs: &[(&str, &str)]) -> Result<String, OrionError> {
        substitute_with(input, "test", Syntax::Plain, env(pairs))
    }

    fn toml(input: &str, pairs: &[(&str, &str)]) -> Result<String, OrionError> {
        substitute_with(input, "test", Syntax::Toml, env(pairs))
    }

    fn msg(err: OrionError) -> String {
        match err {
            OrionError::Config { message } => message,
            other => unreachable!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn simple_substitution() {
        let out = plain("url = ${DB_URL}", &[("DB_URL", "postgres://x")]).expect("test");
        assert_eq!(out, "url = postgres://x");
    }

    #[test]
    fn default_used_when_unset() {
        let out = plain("port = ${PORT:-8080}", &[]).expect("test");
        assert_eq!(out, "port = 8080");
    }

    #[test]
    fn empty_default_is_allowed() {
        let out = plain("v = '${EMPTY:-}'", &[]).expect("test");
        assert_eq!(out, "v = ''");
    }

    /// `:-` is "unset only": a variable exported empty is inserted empty.
    #[test]
    fn a_default_does_not_replace_an_empty_value() {
        let out = plain("v = '${E:-fallback}'", &[("E", "")]).expect("test");
        assert_eq!(out, "v = ''");
    }

    #[test]
    fn missing_required_var_errors() {
        let message = msg(plain("v = ${NOPE}", &[]).expect_err("test"));
        assert!(message.contains("NOPE"), "{message}");
        assert!(message.contains("test:1:5"), "{message}");
    }

    #[test]
    fn dollar_dollar_escapes_to_single_dollar() {
        let out = plain("price = $$5", &[]).expect("test");
        assert_eq!(out, "price = $5");
    }

    #[test]
    fn multiple_substitutions_one_string() {
        let out = plain("${A}/${B}/${C:-fallback}", &[("A", "x"), ("B", "y")]).expect("test");
        assert_eq!(out, "x/y/fallback");
    }

    #[test]
    fn unterminated_brace_errors() {
        let message = msg(plain("v = ${OOPS", &[]).expect_err("test"));
        assert!(message.contains("Unterminated"), "{message}");
        assert!(message.contains("test:1:5"), "{message}");
    }

    #[test]
    fn invalid_var_name_errors() {
        let message = msg(plain("v = ${bad.name}", &[]).expect_err("test"));
        assert!(
            message.contains("Invalid env var name 'bad.name'"),
            "{message}"
        );
        let message = msg(plain("v = ${}", &[]).expect_err("test"));
        assert!(message.contains("Invalid env var name ''"), "{message}");
    }

    #[test]
    fn an_unsupported_operator_is_named() {
        for (input, op) in [
            ("v = ${VAR:+x}", ":+"),
            ("v = ${VAR:=x}", ":="),
            ("v = ${VAR-x}", "-"),
            ("v = ${VAR?x}", "?"),
        ] {
            let message = msg(plain(input, &[("VAR", "1")]).expect_err(input));
            assert!(
                message.contains(&format!("Unsupported substitution '${{VAR{op}…}}'")),
                "{input}: {message}"
            );
            assert!(message.contains("${VAR:?message}"), "{message}");
        }
    }

    #[test]
    fn no_substitution_when_no_dollar() {
        let out = plain("plain text", &[]).expect("test");
        assert_eq!(out, "plain text");
    }

    #[test]
    fn dollar_not_followed_by_brace_is_literal() {
        let out = plain("amount: $5", &[]).expect("test");
        assert_eq!(out, "amount: $5");
    }

    #[test]
    fn substitution_is_not_recursive() {
        // The value of A contains ${B} as a literal string — it must NOT be
        // re-evaluated as another env var. Prevents env-var injection.
        let out = plain("v = ${A}", &[("A", "literal-${B}"), ("B", "secret")]).expect("test");
        assert_eq!(out, "v = literal-${B}");
    }

    /// C4d: the names a config file references are the names Orion reads from
    /// the environment on its behalf, so the unknown-variable guard has to see
    /// them. Every grammar form counts, and a `$$`-escaped dollar does not.
    #[test]
    fn referenced_vars_reports_every_placeholder() {
        let found = referenced_vars(
            "url = \"${ORION_DB_URL}\"\nport = ${PORT:-8080}\nkey = ${KEY:?why}\nprice = $$5\nplain = 1\n",
        );
        assert_eq!(
            found,
            ["KEY", "ORION_DB_URL", "PORT"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    /// A required-but-unset variable must not fail this scan — `substitute`
    /// owns that error, and it runs afterwards with the real environment.
    #[test]
    fn referenced_vars_does_not_require_the_variables_to_be_set() {
        let found = referenced_vars("v = ${DEFINITELY_NOT_SET_ANYWHERE}");
        assert!(found.contains("DEFINITELY_NOT_SET_ANYWHERE"));
    }

    /// A default is never evaluated by the scan, so the old "answer every
    /// lookup" trick would miss the inner name — and the C4d guard would
    /// then refuse `ORION_B` as an unknown setting.
    #[test]
    fn referenced_vars_reports_nested_names() {
        let found = referenced_vars("v = ${ORION_A:-${ORION_B:-${ORION_C:?x}}}");
        assert_eq!(
            found,
            ["ORION_A", "ORION_B", "ORION_C"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[test]
    fn unicode_pass_through() {
        let out = plain("π = ${PI}", &[("PI", "3.14")]).expect("test");
        assert_eq!(out, "π = 3.14");
    }

    // ---- comments (TOML) ----

    #[test]
    fn a_placeholder_in_a_comment_is_not_substituted() {
        let input = "# point it at ${R2_ENDPOINT}\nport = ${PORT:-1}  # or ${OTHER}\n";
        let out = toml(input, &[]).expect("a comment requires nothing");
        assert_eq!(
            out,
            "# point it at ${R2_ENDPOINT}\nport = 1  # or ${OTHER}\n"
        );
    }

    #[test]
    fn a_comment_does_not_make_a_variable_required() {
        assert!(toml("# needs ${UNSET_IN_COMMENT}\nx = 1\n", &[]).is_ok());
        // The same text as a plain blob still substitutes it.
        assert!(plain("# needs ${UNSET_IN_COMMENT}\nx = 1\n", &[]).is_err());
    }

    #[test]
    fn referenced_vars_toml_ignores_comments() {
        let found = referenced_vars_toml("# ${IN_COMMENT}\nurl = \"${IN_VALUE}\" # ${TRAILING}\n");
        assert_eq!(found, ["IN_VALUE"].into_iter().map(String::from).collect());
    }

    #[test]
    fn a_hash_inside_a_basic_string_is_not_a_comment() {
        let out = toml("url = \"http://h/#frag-${X}\"\n", &[("X", "1")]).expect("test");
        assert_eq!(out, "url = \"http://h/#frag-1\"\n");
        // An escaped quote does not close the string.
        let out = toml("v = \"a\\\"#${X}\"\n", &[("X", "1")]).expect("test");
        assert_eq!(out, "v = \"a\\\"#1\"\n");
    }

    #[test]
    fn a_hash_inside_a_literal_string_is_not_a_comment() {
        let out = toml("v = 'a#${X}'\n", &[("X", "1")]).expect("test");
        assert_eq!(out, "v = 'a#1'\n");
    }

    #[test]
    fn a_hash_inside_a_multiline_basic_string_is_not_a_comment() {
        let input = "v = \"\"\"\nline # ${X}\n\\\"\"\" still inside # ${Y}\n\"\"\"\"\n# ${Z}\n";
        let out = toml(input, &[("X", "1"), ("Y", "2")]).expect("test");
        assert_eq!(
            out,
            "v = \"\"\"\nline # 1\n\\\"\"\" still inside # 2\n\"\"\"\"\n# ${Z}\n"
        );
    }

    #[test]
    fn a_hash_inside_a_multiline_literal_string_is_not_a_comment() {
        let input = "v = '''\n# ${X}\n'''\n# ${Y}\n";
        let out = toml(input, &[("X", "1")]).expect("test");
        assert_eq!(out, "v = '''\n# 1\n'''\n# ${Y}\n");
    }

    #[test]
    fn a_hash_inside_a_placeholder_default_is_not_a_comment() {
        let out = toml("color = ${C:-\"#fff\"}\nx = \"${V:-a # b}\"\n", &[]).expect("test");
        assert_eq!(out, "color = \"#fff\"\nx = \"a # b\"\n");
    }

    #[test]
    fn an_unquoted_value_is_still_substituted() {
        let out = toml(
            "cookie_secure = ${COOKIE_SECURE:-true}  # note ${IGNORED}\n",
            &[],
        )
        .expect("test");
        assert_eq!(out, "cookie_secure = true  # note ${IGNORED}\n");
    }

    #[test]
    fn an_unterminated_single_line_string_does_not_swallow_the_file() {
        let out = toml("a = \"oops\n# ${IGNORED}\nb = ${B}\n", &[("B", "2")]).expect("test");
        assert_eq!(out, "a = \"oops\n# ${IGNORED}\nb = 2\n");
    }

    #[test]
    fn dollar_dollar_in_a_comment_is_left_alone() {
        let out = toml("# $${X} and $$\nv = \"$$\"\n", &[]).expect("test");
        assert_eq!(out, "# $${X} and $$\nv = \"$\"\n");
    }

    #[test]
    fn plain_syntax_treats_hash_as_text() {
        let out = plain(r##"{"u":"h#${X}"}"##, &[("X", "1")]).expect("test");
        assert_eq!(out, r##"{"u":"h#1"}"##);
    }

    // ---- ${VAR:?message} ----

    #[test]
    fn required_with_message_fails_when_unset() {
        let message = msg(plain("v = ${DB:?set DB to the state database}", &[]).expect_err("x"));
        assert_eq!(
            message,
            "DB is required: set DB to the state database (test:1:5)"
        );
    }

    #[test]
    fn required_with_message_fails_when_empty() {
        let message = msg(plain("v = ${DB:?set it}", &[("DB", "")]).expect_err("x"));
        assert_eq!(message, "DB is required: set it (test:1:5)");
    }

    #[test]
    fn required_with_message_passes_when_set() {
        let out = plain("v = ${DB:?set it}", &[("DB", "x")]).expect("test");
        assert_eq!(out, "v = x");
    }

    #[test]
    fn an_empty_message_is_allowed() {
        let message = msg(plain("v = ${DB:?}", &[]).expect_err("x"));
        assert_eq!(message, "DB is required (test:1:5)");
    }

    #[test]
    fn the_message_is_evaluated_lazily() {
        // Set: the message — which would itself fail — is never evaluated.
        let out = plain("v = ${DB:?see ${HINT}}", &[("DB", "x")]).expect("test");
        assert_eq!(out, "v = x");
        // Unset: it is evaluated like a default.
        let message = msg(plain("v = ${DB:?see ${HINT:-the docs}}", &[]).expect_err("x"));
        assert_eq!(message, "DB is required: see the docs (test:1:5)");
    }

    // ---- nesting ----

    #[test]
    fn a_nested_default_resolves_inner_first() {
        let input = "v = ${A:-${B:-c}}";
        assert_eq!(
            plain(input, &[("A", "a"), ("B", "b")]).expect("substitutes"),
            "v = a"
        );
        assert_eq!(plain(input, &[("B", "b")]).expect("substitutes"), "v = b");
        assert_eq!(plain(input, &[]).expect("substitutes"), "v = c");
    }

    #[test]
    fn an_inner_required_variable_is_only_required_when_reached() {
        assert_eq!(
            plain("v = ${A:-${B}}", &[("A", "a")]).expect("substitutes"),
            "v = a"
        );
        let message = msg(plain("v = ${A:-${B}}", &[]).expect_err("x"));
        assert!(message.contains("'B' is not set"), "{message}");
        assert!(message.contains("test:1:10"), "{message}");
    }

    #[test]
    fn nesting_deeper_than_the_cap_is_an_error() {
        let ok = format!(
            "{}x{}",
            "${A:-".repeat(MAX_NESTING),
            "}".repeat(MAX_NESTING)
        );
        assert_eq!(plain(&ok, &[]).expect("substitutes"), "x");
        let deep = format!(
            "{}x{}",
            "${A:-".repeat(MAX_NESTING + 1),
            "}".repeat(MAX_NESTING + 1)
        );
        let message = msg(plain(&deep, &[]).expect_err("too deep"));
        assert!(message.contains("nested deeper than 8 levels"), "{message}");
    }

    #[test]
    fn an_unbalanced_nested_placeholder_names_its_position() {
        let message = msg(plain("a = 1\nv = ${A:-${B:-x}", &[]).expect_err("x"));
        assert!(message.contains("Unterminated"), "{message}");
        assert!(message.contains("test:2:5"), "{message}");
    }

    #[test]
    fn a_json_default_with_bare_braces_is_unchanged() {
        let out = plain(r#"v = ${A:-{"a":1}}"#, &[]).expect("test");
        assert_eq!(out, r#"v = {"a":1}"#);
    }

    #[test]
    fn errors_carry_line_and_column() {
        let message = msg(toml("a = 1\n# ${X}\nπé = ${MISSING}\n", &[]).expect_err("x"));
        // Columns count characters: "πé = " is five of them.
        assert!(message.contains("test:3:6"), "{message}");
    }
}
