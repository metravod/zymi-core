//! `when:` expression parser and evaluator (ADR-0028).
//!
//! Grammar:
//! ```text
//! EXPR := COMP (LOGIC COMP)*
//! COMP := VALUE OP VALUE
//! OP    := '==' | '!='
//! LOGIC := '&&' | '||'
//! VALUE := single-quoted-string | unquoted-token
//! ```
//!
//! `&&` and `||` have equal precedence and evaluate left-to-right.
//! Template substitution (`${inputs.*}`, `${steps.*.output}`) is the caller's
//! responsibility — the parser sees the resolved string. Comparison is
//! byte-exact, no normalisation, no case-folding.

use std::collections::HashSet;
use std::fmt;

/// Parse + evaluate a `when:` expression that has already had `${...}`
/// templates resolved.
pub fn evaluate(expr: &str) -> Result<bool, WhenExprError> {
    let mut p = Parser::new(expr);
    let value = p.parse_expr()?;
    p.skip_ws();
    if !p.eof() {
        return Err(WhenExprError::Trailing {
            at: p.pos,
            rest: p.input[p.pos..].to_string(),
        });
    }
    Ok(value)
}

/// Parse-only: validate syntax without evaluating. Used at config-load time.
pub fn parse_only(expr: &str) -> Result<(), WhenExprError> {
    // Re-use the parser but feed it neutral values — both sides of every
    // comparison are inert tokens, so the structural check still fires.
    // We don't need the boolean answer.
    let mut p = Parser::new(expr);
    let _ = p.parse_expr()?;
    p.skip_ws();
    if !p.eof() {
        return Err(WhenExprError::Trailing {
            at: p.pos,
            rest: p.input[p.pos..].to_string(),
        });
    }
    Ok(())
}

/// Collect step ids referenced via `${steps.<id>.output}` in a `when:` source.
///
/// Used by validation (ADR-0028 §Validation) to enforce that every step a
/// `when:` reads is also in `depends_on`. Lexical scan — does not need
/// expression to be syntactically valid.
pub fn collect_step_refs(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i + 8 < bytes.len() {
        if &bytes[i..i + 8] == b"${steps." {
            let start = i + 8;
            if let Some(end_rel) = expr[start..].find('}') {
                let inner = &expr[start..start + end_rel];
                if let Some(id) = inner.strip_suffix(".output") {
                    if !id.is_empty() && seen.insert(id.to_string()) {
                        out.push(id.to_string());
                    }
                }
                i = start + end_rel + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WhenExprError {
    #[error("expected comparison operator (`==` or `!=`) at position {at}")]
    ExpectedOp { at: usize },
    #[error("expected value at position {at}")]
    ExpectedValue { at: usize },
    #[error("unterminated string starting at position {at}")]
    UnterminatedString { at: usize },
    #[error("unexpected `{token}` at position {at} (expected `&&` or `||`)")]
    BadLogic { at: usize, token: String },
    #[error("unexpected trailing input `{rest}` at position {at}")]
    Trailing { at: usize, rest: String },
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_expr(&mut self) -> Result<bool, WhenExprError> {
        let mut acc = self.parse_comp()?;
        loop {
            self.skip_ws();
            if self.eof() {
                break;
            }
            // Peek a 2-byte operator.
            let bytes = self.input.as_bytes();
            if self.pos + 2 > bytes.len() {
                // Trailing garbage — let the outer evaluate() error on it.
                break;
            }
            let op = &bytes[self.pos..self.pos + 2];
            match op {
                b"&&" => {
                    self.pos += 2;
                    let rhs = self.parse_comp()?;
                    acc = acc && rhs;
                }
                b"||" => {
                    self.pos += 2;
                    let rhs = self.parse_comp()?;
                    acc = acc || rhs;
                }
                _ => {
                    // A leading `&` or `|` alone is a syntax error; report it
                    // explicitly so users don't see a generic "trailing input".
                    if op[0] == b'&' || op[0] == b'|' {
                        return Err(WhenExprError::BadLogic {
                            at: self.pos,
                            token: String::from_utf8_lossy(op).into_owned(),
                        });
                    }
                    break;
                }
            }
        }
        Ok(acc)
    }

    fn parse_comp(&mut self) -> Result<bool, WhenExprError> {
        let lhs = self.parse_value()?;
        self.skip_ws();
        let bytes = self.input.as_bytes();
        if self.pos + 2 > bytes.len() {
            return Err(WhenExprError::ExpectedOp { at: self.pos });
        }
        let op = &bytes[self.pos..self.pos + 2];
        let is_eq = match op {
            b"==" => true,
            b"!=" => false,
            _ => return Err(WhenExprError::ExpectedOp { at: self.pos }),
        };
        self.pos += 2;
        let rhs = self.parse_value()?;
        Ok(if is_eq { lhs == rhs } else { lhs != rhs })
    }

    fn parse_value(&mut self) -> Result<String, WhenExprError> {
        self.skip_ws();
        let start = self.pos;
        let bytes = self.input.as_bytes();
        if self.pos >= bytes.len() {
            return Err(WhenExprError::ExpectedValue { at: start });
        }
        if bytes[self.pos] == b'\'' {
            // Single-quoted string; no escapes (YAGNI for this surface).
            self.pos += 1;
            let value_start = self.pos;
            while self.pos < bytes.len() && bytes[self.pos] != b'\'' {
                self.pos += 1;
            }
            if self.pos >= bytes.len() {
                return Err(WhenExprError::UnterminatedString { at: start });
            }
            let value = self.input[value_start..self.pos].to_string();
            self.pos += 1; // consume closing quote
            return Ok(value);
        }
        // Unquoted token: read until whitespace, op-start, or logic-start.
        let value_start = self.pos;
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            if b.is_ascii_whitespace() {
                break;
            }
            // Stop before any 2-byte operator we recognise.
            if self.pos + 2 <= bytes.len() {
                let two = &bytes[self.pos..self.pos + 2];
                if matches!(two, b"==" | b"!=" | b"&&" | b"||") {
                    break;
                }
            }
            self.pos += 1;
        }
        if self.pos == value_start {
            return Err(WhenExprError::ExpectedValue { at: start });
        }
        Ok(self.input[value_start..self.pos].to_string())
    }
}

impl fmt::Display for Parser<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Parser@{}", self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_eq_true() {
        assert!(evaluate("'rag' == 'rag'").unwrap());
    }

    #[test]
    fn simple_eq_false() {
        assert!(!evaluate("'rag' == 'short'").unwrap());
    }

    #[test]
    fn ne_true() {
        assert!(evaluate("'a' != 'b'").unwrap());
    }

    #[test]
    fn unquoted_tokens() {
        // Resolved templates land as bare tokens.
        assert!(evaluate("rag == rag").unwrap());
        assert!(!evaluate("rag == short").unwrap());
    }

    #[test]
    fn whitespace_tolerant() {
        assert!(evaluate("   'a'   ==   'a'   ").unwrap());
    }

    #[test]
    fn and_left_to_right() {
        assert!(evaluate("'a' == 'a' && 'b' == 'b'").unwrap());
        assert!(!evaluate("'a' == 'a' && 'b' == 'c'").unwrap());
    }

    #[test]
    fn or_left_to_right() {
        assert!(evaluate("'a' == 'b' || 'c' == 'c'").unwrap());
        assert!(!evaluate("'a' == 'b' || 'c' == 'd'").unwrap());
    }

    #[test]
    fn equal_precedence_left_to_right() {
        // (a==a) || (b==b) && (c==d)
        //   = true || (true && false)         if right-assoc
        //   = (true || true) && false = false if left-to-right
        // Spec says left-to-right.
        assert!(!evaluate("'a' == 'a' || 'b' == 'b' && 'c' == 'd'").unwrap());
    }

    #[test]
    fn empty_string_value() {
        assert!(evaluate("'' == ''").unwrap());
        assert!(!evaluate("'' == 'x'").unwrap());
    }

    #[test]
    fn missing_op_errors() {
        assert!(matches!(
            evaluate("'a' 'b'"),
            Err(WhenExprError::ExpectedOp { .. })
        ));
    }

    #[test]
    fn missing_value_errors() {
        assert!(matches!(
            evaluate("'a' =="),
            Err(WhenExprError::ExpectedValue { .. })
        ));
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(matches!(
            evaluate("'a == 'b'"),
            Err(WhenExprError::UnterminatedString { .. })
                | Err(WhenExprError::ExpectedOp { .. })
        ));
    }

    #[test]
    fn trailing_garbage_errors() {
        assert!(matches!(
            evaluate("'a' == 'a' foo"),
            Err(WhenExprError::Trailing { .. })
        ));
    }

    #[test]
    fn parse_only_accepts_valid() {
        assert!(parse_only("'a' == 'b' && 'c' != 'd'").is_ok());
    }

    #[test]
    fn parse_only_rejects_invalid() {
        assert!(parse_only("'a' == ").is_err());
    }

    #[test]
    fn collect_refs_simple() {
        let refs = collect_step_refs("${steps.concierge.output} == 'rag'");
        assert_eq!(refs, vec!["concierge"]);
    }

    #[test]
    fn collect_refs_multiple() {
        let refs = collect_step_refs(
            "${steps.a.output} == 'x' && ${steps.b.output} != ${steps.a.output}",
        );
        assert_eq!(refs, vec!["a", "b"]);
    }

    #[test]
    fn collect_refs_ignores_non_output() {
        // Future field accessors won't match the .output suffix and are skipped.
        let refs = collect_step_refs("${steps.x.route} == 'r'");
        assert!(refs.is_empty());
    }
}
