//! Haystack filter parser and evaluator.
//!
//! Supports a subset of the Haystack 3.0 filter syntax:
//! - Marker checks: `point`, `analog`, `enabled`
//! - Equality: `channel==1113`, `unit=="°F"`
//! - Comparison: `channel > 1000`, `cur < 100`
//! - Compound: `point and temp`, `analog or digital`
//! - Negation: `not disabled`
//!
//! Grammar (recursive descent):
//! ```text
//! expr   = andExpr ("or" andExpr)*
//! andExpr = term ("and" term)*
//! term   = "not" term | "(" expr ")" | cmp
//! cmp    = path (("==" | "!=" | "<" | "<=" | ">" | ">=") value)?
//! path   = name ("->" name)*
//! value  = NUMBER | QUOTED_STRING | "true" | "false"
//! ```

use std::collections::HashMap;

use sandstar_ipc::types::ChannelInfo;

use crate::sox::dyn_slots::DynValue;

// ── AST ─────────────────────────────────────────────────────

/// Filter expression AST node.
#[derive(Debug, Clone)]
pub enum Expr {
    /// Tag is present (marker check).
    Has(String),
    /// Tag is absent.
    Missing(String),
    /// Comparison: path op value.
    Cmp(String, CmpOp, Value),
    /// Logical AND.
    And(Box<Expr>, Box<Expr>),
    /// Logical OR.
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
pub enum Value {
    Num(f64),
    Str(String),
    Bool(bool),
}

// ── Parser ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Num(f64),
    Str(String),
    And,
    Or,
    Not,
    Eq, // ==
    Ne, // !=
    Lt, // <
    Le, // <=
    Gt, // >
    Ge, // >=
    LParen,
    RParen,
    True,
    False,
}

/// Maximum recursion depth for filter parsing (prevents stack overflow DoS).
const MAX_PARSE_DEPTH: usize = 32;

/// Parse a Haystack filter string into an AST.
pub fn parse(input: &str) -> Result<Expr, String> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err("empty filter".into());
    }
    let mut pos = 0;
    let result = parse_or(&tokens, &mut pos, 0)?;
    if pos < tokens.len() {
        return Err(format!("unexpected token at position {}", pos));
    }
    Ok(result)
}

fn parse_or(tokens: &[Token], pos: &mut usize, depth: usize) -> Result<Expr, String> {
    if depth > MAX_PARSE_DEPTH {
        return Err(format!(
            "filter too deeply nested (max {} levels)",
            MAX_PARSE_DEPTH
        ));
    }
    let mut left = parse_and(tokens, pos, depth)?;
    while *pos < tokens.len() && tokens[*pos] == Token::Or {
        *pos += 1;
        let right = parse_and(tokens, pos, depth)?;
        left = Expr::Or(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_and(tokens: &[Token], pos: &mut usize, depth: usize) -> Result<Expr, String> {
    let mut left = parse_term(tokens, pos, depth)?;
    while *pos < tokens.len() && tokens[*pos] == Token::And {
        *pos += 1;
        let right = parse_term(tokens, pos, depth)?;
        left = Expr::And(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_term(tokens: &[Token], pos: &mut usize, depth: usize) -> Result<Expr, String> {
    if *pos >= tokens.len() {
        return Err("unexpected end of filter".into());
    }

    // not term
    if tokens[*pos] == Token::Not {
        *pos += 1;
        let inner = parse_term(tokens, pos, depth)?;
        return Ok(match inner {
            Expr::Has(name) => Expr::Missing(name),
            other => Expr::Missing(format!("{:?}", other)),
        });
    }

    // ( expr )
    if tokens[*pos] == Token::LParen {
        *pos += 1;
        let inner = parse_or(tokens, pos, depth + 1)?;
        if *pos >= tokens.len() || tokens[*pos] != Token::RParen {
            return Err("missing closing parenthesis".into());
        }
        *pos += 1;
        return Ok(inner);
    }

    // cmp: name [op value]
    parse_cmp(tokens, pos)
}

fn parse_cmp(tokens: &[Token], pos: &mut usize) -> Result<Expr, String> {
    let name = match &tokens[*pos] {
        Token::Name(n) => n.clone(),
        other => return Err(format!("expected tag name, got {:?}", other)),
    };
    *pos += 1;

    // Check for comparison operator
    if *pos >= tokens.len() {
        return Ok(Expr::Has(name));
    }

    let op = match &tokens[*pos] {
        Token::Eq => CmpOp::Eq,
        Token::Ne => CmpOp::Ne,
        Token::Lt => CmpOp::Lt,
        Token::Le => CmpOp::Le,
        Token::Gt => CmpOp::Gt,
        Token::Ge => CmpOp::Ge,
        _ => return Ok(Expr::Has(name)), // no operator = marker check
    };
    *pos += 1;

    // Parse value
    if *pos >= tokens.len() {
        return Err("expected value after operator".into());
    }
    let val = match &tokens[*pos] {
        Token::Num(n) => Value::Num(*n),
        Token::Str(s) => Value::Str(s.clone()),
        Token::True => Value::Bool(true),
        Token::False => Value::Bool(false),
        other => return Err(format!("expected value, got {:?}", other)),
    };
    *pos += 1;

    Ok(Expr::Cmp(name, op, val))
}

/// Tokenize a filter string.
fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Skip whitespace
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        // Parentheses
        if chars[i] == '(' {
            tokens.push(Token::LParen);
            i += 1;
            continue;
        }
        if chars[i] == ')' {
            tokens.push(Token::RParen);
            i += 1;
            continue;
        }

        // Operators
        if chars[i] == '=' && i + 1 < chars.len() && chars[i + 1] == '=' {
            tokens.push(Token::Eq);
            i += 2;
            continue;
        }
        if chars[i] == '!' && i + 1 < chars.len() && chars[i + 1] == '=' {
            tokens.push(Token::Ne);
            i += 2;
            continue;
        }
        if chars[i] == '<' {
            if i + 1 < chars.len() && chars[i + 1] == '=' {
                tokens.push(Token::Le);
                i += 2;
            } else {
                tokens.push(Token::Lt);
                i += 1;
            }
            continue;
        }
        if chars[i] == '>' {
            if i + 1 < chars.len() && chars[i + 1] == '=' {
                tokens.push(Token::Ge);
                i += 2;
            } else {
                tokens.push(Token::Gt);
                i += 1;
            }
            continue;
        }

        // Quoted string
        if chars[i] == '"' {
            i += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                s.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1; // skip closing quote
            }
            tokens.push(Token::Str(s));
            continue;
        }

        // Number (with optional negative sign)
        if chars[i].is_ascii_digit()
            || (chars[i] == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            let start = i;
            if chars[i] == '-' {
                i += 1;
            }
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let num_str: String = chars[start..i].iter().collect();
            let n: f64 = num_str
                .parse()
                .map_err(|_| format!("invalid number: {}", num_str))?;
            tokens.push(Token::Num(n));
            continue;
        }

        // Name / keyword
        if chars[i].is_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "and" => tokens.push(Token::And),
                "or" => tokens.push(Token::Or),
                "not" => tokens.push(Token::Not),
                "true" => tokens.push(Token::True),
                "false" => tokens.push(Token::False),
                _ => tokens.push(Token::Name(word)),
            }
            continue;
        }

        return Err(format!("unexpected character: '{}'", chars[i]));
    }

    Ok(tokens)
}

// ── Evaluator ───────────────────────────────────────────────

/// Evaluate a filter expression against a ChannelInfo.
pub fn matches(expr: &Expr, ch: &ChannelInfo) -> bool {
    match expr {
        Expr::Has(tag) => has_tag(ch, tag),
        Expr::Missing(tag) => !has_tag(ch, tag),
        Expr::Cmp(tag, op, val) => cmp_tag(ch, tag, *op, val),
        Expr::And(l, r) => matches(l, ch) && matches(r, ch),
        Expr::Or(l, r) => matches(l, ch) || matches(r, ch),
    }
}

/// Evaluate a filter expression against a ChannelInfo with dynamic tag fallback.
///
/// First checks static ChannelInfo fields, then falls back to the dynamic tag
/// dictionary. This allows filters like `modbusAddr==40001` to match components
/// that have `modbusAddr` set as a dynamic tag.
pub fn matches_with_tags(
    expr: &Expr,
    ch: &ChannelInfo,
    tags: Option<&HashMap<String, DynValue>>,
) -> bool {
    match tags {
        None => matches(expr, ch),
        Some(tags) => matches_with_tags_inner(expr, ch, tags),
    }
}

fn matches_with_tags_inner(
    expr: &Expr,
    ch: &ChannelInfo,
    tags: &HashMap<String, DynValue>,
) -> bool {
    match expr {
        Expr::Has(tag) => has_tag(ch, tag) || has_dyn_tag(tags, tag),
        Expr::Missing(tag) => !has_tag(ch, tag) && !has_dyn_tag(tags, tag),
        Expr::Cmp(tag, op, val) => {
            if cmp_tag(ch, tag, *op, val) {
                return true;
            }
            cmp_dyn_tag(tags, tag, *op, val)
        }
        Expr::And(l, r) => {
            matches_with_tags_inner(l, ch, tags) && matches_with_tags_inner(r, ch, tags)
        }
        Expr::Or(l, r) => {
            matches_with_tags_inner(l, ch, tags) || matches_with_tags_inner(r, ch, tags)
        }
    }
}

/// Check if a dynamic tag exists (marker-style presence check).
fn has_dyn_tag(tags: &HashMap<String, DynValue>, tag: &str) -> bool {
    match tags.get(tag) {
        Some(DynValue::Null) => false, // Null means absent
        Some(_) => true,
        None => false,
    }
}

/// Compare a dynamic tag value against a filter value.
fn cmp_dyn_tag(tags: &HashMap<String, DynValue>, tag: &str, op: CmpOp, val: &Value) -> bool {
    let Some(dyn_val) = tags.get(tag) else {
        return false;
    };
    match (dyn_val, val) {
        // Numeric comparisons: DynValue::Int or Float vs filter Num
        (DynValue::Int(i), Value::Num(n)) => cmp_f64(*i as f64, op, *n),
        (DynValue::Float(f), Value::Num(n)) => cmp_f64(*f, op, *n),
        // String comparisons: DynValue::Str or Ref vs filter Str
        (DynValue::Str(s), Value::Str(v)) => cmp_str(s, op, v),
        (DynValue::Ref(r), Value::Str(v)) => cmp_str(r, op, v),
        // Bool comparisons
        (DynValue::Bool(b), Value::Bool(v)) => match op {
            CmpOp::Eq => b == v,
            CmpOp::Ne => b != v,
            _ => false,
        },
        // Numeric string matching: try to compare DynValue::Str as number
        (DynValue::Str(s), Value::Num(n)) => s
            .parse::<f64>()
            .map(|f| cmp_f64(f, op, *n))
            .unwrap_or(false),
        _ => false,
    }
}

/// Check if a channel "has" a tag (marker-style).
fn has_tag(ch: &ChannelInfo, tag: &str) -> bool {
    match tag {
        // Well-known tags mapped to ChannelInfo fields
        "point" => true, // all channels are points
        "enabled" => ch.enabled,
        "disabled" => !ch.enabled,
        "analog" | "Analog" => ch.channel_type.eq_ignore_ascii_case("analog"),
        "digital" | "Digital" => ch.channel_type.eq_ignore_ascii_case("digital"),
        "pwm" | "Pwm" => ch.channel_type.eq_ignore_ascii_case("pwm"),
        "i2c" | "I2c" | "I2C" => ch.channel_type.eq_ignore_ascii_case("i2c"),
        "uart" | "Uart" | "UART" => ch.channel_type.eq_ignore_ascii_case("uart"),
        "virtual" | "Virtual" => {
            ch.channel_type.eq_ignore_ascii_case("virtualanalog")
                || ch.channel_type.eq_ignore_ascii_case("virtualdigital")
        }
        "input" | "in" | "In" => ch.direction.eq_ignore_ascii_case("in"),
        "output" | "out" | "Out" => ch.direction.eq_ignore_ascii_case("out"),
        // Substring match on label as last resort
        _ => ch.label.to_lowercase().contains(&tag.to_lowercase()),
    }
}

/// Compare a tag value against a filter value.
fn cmp_tag(ch: &ChannelInfo, tag: &str, op: CmpOp, val: &Value) -> bool {
    match tag {
        "channel" | "id" => {
            if let Value::Num(n) = val {
                cmp_f64(ch.id as f64, op, *n)
            } else {
                false
            }
        }
        "cur" | "curVal" => {
            if let Value::Num(n) = val {
                cmp_f64(ch.cur, op, *n)
            } else {
                false
            }
        }
        "raw" | "rawVal" => {
            if let Value::Num(n) = val {
                cmp_f64(ch.raw, op, *n)
            } else {
                false
            }
        }
        "label" | "navName" | "dis" => {
            if let Value::Str(s) = val {
                cmp_str(&ch.label, op, s)
            } else {
                false
            }
        }
        "type" | "channelType" => {
            if let Value::Str(s) = val {
                cmp_str(&ch.channel_type, op, s)
            } else {
                false
            }
        }
        "direction" => {
            if let Value::Str(s) = val {
                cmp_str(&ch.direction, op, s)
            } else {
                false
            }
        }
        "enabled" => {
            if let Value::Bool(b) = val {
                match op {
                    CmpOp::Eq => ch.enabled == *b,
                    CmpOp::Ne => ch.enabled != *b,
                    _ => false,
                }
            } else {
                false
            }
        }
        "status" => {
            if let Value::Str(s) = val {
                cmp_str(&ch.status, op, s)
            } else {
                false
            }
        }
        _ => false,
    }
}

fn cmp_f64(lhs: f64, op: CmpOp, rhs: f64) -> bool {
    match op {
        CmpOp::Eq => (lhs - rhs).abs() < f64::EPSILON,
        CmpOp::Ne => (lhs - rhs).abs() >= f64::EPSILON,
        CmpOp::Lt => lhs < rhs,
        CmpOp::Le => lhs <= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Ge => lhs >= rhs,
    }
}

fn cmp_str(lhs: &str, op: CmpOp, rhs: &str) -> bool {
    match op {
        CmpOp::Eq => lhs.eq_ignore_ascii_case(rhs),
        CmpOp::Ne => !lhs.eq_ignore_ascii_case(rhs),
        CmpOp::Lt => lhs < rhs,
        CmpOp::Le => lhs <= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Ge => lhs >= rhs,
    }
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_channel(id: u32, label: &str, ct: &str, dir: &str) -> ChannelInfo {
        ChannelInfo {
            id,
            label: label.into(),
            channel_type: ct.into(),
            direction: dir.into(),
            enabled: true,
            status: "Ok".into(),
            cur: 72.5,
            raw: 2048.0,
        }
    }

    #[test]
    fn test_parse_marker() {
        let expr = parse("point").unwrap();
        assert!(matches!(&expr, Expr::Has(n) if n == "point"));
    }

    #[test]
    fn test_parse_equality() {
        let expr = parse("channel==1113").unwrap();
        assert!(
            matches!(&expr, Expr::Cmp(n, CmpOp::Eq, Value::Num(v)) if n == "channel" && *v == 1113.0)
        );
    }

    #[test]
    fn test_parse_string_equality() {
        let expr = parse("unit==\"°F\"").unwrap();
        assert!(
            matches!(&expr, Expr::Cmp(n, CmpOp::Eq, Value::Str(s)) if n == "unit" && s == "°F")
        );
    }

    #[test]
    fn test_parse_and() {
        let expr = parse("point and analog").unwrap();
        assert!(matches!(&expr, Expr::And(_, _)));
    }

    #[test]
    fn test_parse_or() {
        let expr = parse("analog or digital").unwrap();
        assert!(matches!(&expr, Expr::Or(_, _)));
    }

    #[test]
    fn test_parse_not() {
        let expr = parse("not disabled").unwrap();
        assert!(matches!(&expr, Expr::Missing(n) if n == "disabled"));
    }

    #[test]
    fn test_parse_comparison_gt() {
        let expr = parse("channel > 1000").unwrap();
        assert!(
            matches!(&expr, Expr::Cmp(n, CmpOp::Gt, Value::Num(v)) if n == "channel" && *v == 1000.0)
        );
    }

    #[test]
    fn test_parse_compound() {
        let expr = parse("point and channel > 600 and channel < 700").unwrap();
        // Should parse as (point and (channel > 600)) and (channel < 700)
        assert!(matches!(&expr, Expr::And(_, _)));
    }

    #[test]
    fn test_parse_parens() {
        let expr = parse("(analog or digital) and enabled").unwrap();
        assert!(matches!(&expr, Expr::And(_, _)));
    }

    #[test]
    fn test_eval_marker_point() {
        let ch = test_channel(1113, "AI1 Thermistor", "Analog", "In");
        let expr = parse("point").unwrap();
        assert!(matches(&expr, &ch));
    }

    #[test]
    fn test_eval_marker_analog() {
        let ch = test_channel(1113, "AI1 Thermistor", "Analog", "In");
        assert!(matches(&parse("analog").unwrap(), &ch));
        assert!(!matches(&parse("digital").unwrap(), &ch));
    }

    #[test]
    fn test_eval_channel_eq() {
        let ch = test_channel(1113, "AI1", "Analog", "In");
        assert!(matches(&parse("channel==1113").unwrap(), &ch));
        assert!(!matches(&parse("channel==612").unwrap(), &ch));
    }

    #[test]
    fn test_eval_channel_range() {
        let ch = test_channel(1113, "AI1", "Analog", "In");
        assert!(matches(&parse("channel > 1000").unwrap(), &ch));
        assert!(!matches(&parse("channel < 1000").unwrap(), &ch));
    }

    #[test]
    fn test_eval_and() {
        let ch = test_channel(1113, "AI1", "Analog", "In");
        assert!(matches(&parse("analog and input").unwrap(), &ch));
        assert!(!matches(&parse("analog and output").unwrap(), &ch));
    }

    #[test]
    fn test_eval_or() {
        let ch = test_channel(1113, "AI1", "Analog", "In");
        assert!(matches(&parse("analog or digital").unwrap(), &ch));
        assert!(matches(&parse("digital or analog").unwrap(), &ch));
        assert!(!matches(&parse("digital or pwm").unwrap(), &ch));
    }

    #[test]
    fn test_eval_not() {
        let ch = test_channel(1113, "AI1", "Analog", "In");
        assert!(matches(&parse("not disabled").unwrap(), &ch)); // ch.enabled=true
        assert!(!matches(&parse("not enabled").unwrap(), &ch));
    }

    #[test]
    fn test_eval_label_contains() {
        let ch = test_channel(1113, "AI1 Thermistor 10K", "Analog", "In");
        assert!(matches(&parse("thermistor").unwrap(), &ch));
        assert!(!matches(&parse("humidity").unwrap(), &ch));
    }

    #[test]
    fn test_eval_label_eq() {
        let ch = test_channel(612, "I2C SDP610 CFM", "I2c", "In");
        assert!(matches(&parse("dis==\"I2C SDP610 CFM\"").unwrap(), &ch));
    }

    #[test]
    fn test_eval_complex() {
        let ch = test_channel(612, "I2C SDP610 CFM", "I2c", "In");
        assert!(matches(
            &parse("i2c and channel > 600 and channel < 700").unwrap(),
            &ch
        ));
    }

    #[test]
    fn test_parse_depth_limit() {
        // 33 levels of nesting exceeds MAX_PARSE_DEPTH (32)
        let deep = "(".repeat(33) + "point" + &")".repeat(33);
        let err = parse(&deep).unwrap_err();
        assert!(
            err.contains("deeply nested"),
            "expected depth error, got: {}",
            err
        );
    }

    #[test]
    fn test_parse_error_empty() {
        assert!(parse("").is_err());
    }

    #[test]
    fn test_parse_error_trailing() {
        assert!(parse("point )").is_err());
    }

    // ════════════════════════════════════════════════════════════
    // Filter parser DoS protection tests
    // ════════════════════════════════════════════════════════════

    #[test]
    fn test_filter_depth_limit_exactly_32() {
        // Exactly 32 levels of parenthesized nesting — should parse OK
        let deep = "(".repeat(32) + "point" + &")".repeat(32);
        let result = parse(&deep);
        assert!(
            result.is_ok(),
            "32-deep should parse OK, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_filter_depth_limit_33_rejected() {
        // 33 levels exceeds MAX_PARSE_DEPTH
        let deep = "(".repeat(33) + "point" + &")".repeat(33);
        let result = parse(&deep);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("deeply nested") || err.contains("max"),
            "expected depth error, got: {}",
            err
        );
    }

    #[test]
    fn test_filter_deeply_nested_and_chain() {
        // "a and (b and (c and (d and ...)))" — 40 levels of parens
        let mut filter = String::new();
        for i in 0..40 {
            filter.push_str(&format!("t{} and (", i));
        }
        filter.push_str("leaf");
        filter.push_str(&")".repeat(40));

        let result = parse(&filter);
        assert!(result.is_err(), "40-deep and chain should be rejected");
        let err = result.unwrap_err();
        assert!(
            err.contains("deeply nested") || err.contains("max"),
            "expected depth error, got: {}",
            err
        );
    }

    #[test]
    fn test_filter_deeply_nested_or_chain() {
        // "a or (b or (c or (d or ...)))" — 40 levels of parens
        let mut filter = String::new();
        for i in 0..40 {
            filter.push_str(&format!("t{} or (", i));
        }
        filter.push_str("leaf");
        filter.push_str(&")".repeat(40));

        let result = parse(&filter);
        assert!(result.is_err(), "40-deep or chain should be rejected");
        let err = result.unwrap_err();
        assert!(
            err.contains("deeply nested") || err.contains("max"),
            "expected depth error, got: {}",
            err
        );
    }

    #[test]
    fn test_filter_deeply_nested_not_chain() {
        // "not not not not ... disabled" — moderate depth.
        // `not` in parse_term recurses (calls parse_term again), so we keep
        // this within a safe depth that won't overflow the debug-mode stack.
        let mut filter = String::new();
        for _ in 0..20 {
            filter.push_str("not ");
        }
        filter.push_str("disabled");

        // Should parse OK: 20 levels of not-recursion is within stack limits.
        // Even number of nots = Has("disabled"), odd = Missing("disabled").
        let result = parse(&filter);
        assert!(
            result.is_ok(),
            "20-deep not chain should parse, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_filter_empty_string() {
        let result = parse("");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("empty"),
            "expected 'empty' error, got: {}",
            err
        );
    }

    #[test]
    fn test_filter_whitespace_only() {
        let result = parse("   \t  \n  ");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("empty"),
            "expected 'empty' error for whitespace-only input, got: {}",
            err
        );
    }

    #[test]
    fn test_filter_very_long_string() {
        // Large filter string of repeated "a and " — verify no panic.
        // We use a moderate size (1000 terms) to stay within stack limits
        // while still testing the parser's handling of long inputs.
        let segment = "a and ";
        let repetitions = 1000;
        let mut filter = segment.repeat(repetitions);
        filter.push_str("z"); // terminate the chain

        // This should parse OK (the and-chain is iterative, not recursive).
        let result = parse(&filter);
        assert!(result.is_ok(), "1000-term and-chain should parse OK");
    }

    #[test]
    fn test_filter_special_characters_in_tag() {
        // Tags with underscores and numbers should be valid names
        assert!(parse("zone_temp").is_ok());
        assert!(parse("ai1").is_ok());
        assert!(parse("sensor_1_value").is_ok());
        assert!(parse("_leading_underscore").is_ok());

        // Tag with hyphen is NOT valid in the tokenizer (alphanumeric + underscore only)
        let result = parse("zone-temp");
        assert!(result.is_err(), "hyphenated tag should fail tokenization");
    }

    #[test]
    fn test_filter_unicode_values() {
        // Unicode in quoted string values should parse fine
        let result = parse("dis==\"\u{6e29}\u{5ea6}\""); // 温度
        assert!(
            result.is_ok(),
            "unicode string value should parse: {:?}",
            result.err()
        );

        if let Ok(Expr::Cmp(tag, CmpOp::Eq, Value::Str(s))) = &result {
            assert_eq!(tag, "dis");
            assert_eq!(s, "\u{6e29}\u{5ea6}");
        } else {
            panic!("expected Cmp(dis, Eq, Str), got: {:?}", result);
        }
    }

    #[test]
    fn test_filter_null_bytes() {
        // Filter containing \0 bytes — should return clean error, not panic
        let filter = "point\0and\0analog";
        let result = parse(filter);
        // The tokenizer treats \0 as an unexpected character
        assert!(result.is_err(), "null bytes should cause parse error");
    }

    #[test]
    fn test_filter_depth_limit_boundary_31_ok() {
        // 31 levels of nesting — well within limit
        let deep = "(".repeat(31) + "point" + &")".repeat(31);
        let result = parse(&deep);
        assert!(
            result.is_ok(),
            "31-deep should parse OK, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_filter_depth_limit_50_rejected() {
        // 50 levels — well over limit, verify it's caught
        let deep = "(".repeat(50) + "point" + &")".repeat(50);
        let result = parse(&deep);
        assert!(result.is_err(), "50-deep nesting should be rejected");
    }

    #[test]
    fn test_filter_mixed_deep_nesting_rejected() {
        // Mixed and/or with deep parens: (a or (b and (c or ...)))
        let mut filter = String::new();
        for i in 0..35 {
            let op = if i % 2 == 0 { "or" } else { "and" };
            filter.push_str(&format!("t{} {} (", i, op));
        }
        filter.push_str("leaf");
        filter.push_str(&")".repeat(35));

        let result = parse(&filter);
        assert!(result.is_err(), "35-deep mixed nesting should be rejected");
    }

    // ════════════════════════════════════════════════════════════
    // Phase 5.8i — Filter parser stress & edge case tests
    // ════════════════════════════════════════════════════════════

    #[test]
    fn test_unicode_chinese_in_filter_value() {
        // Full Chinese string: 温度传感器
        let result = parse("dis==\"\u{6e29}\u{5ea6}\u{4f20}\u{611f}\u{5668}\"");
        assert!(
            result.is_ok(),
            "Chinese string should parse: {:?}",
            result.err()
        );
        if let Ok(Expr::Cmp(tag, CmpOp::Eq, Value::Str(s))) = &result {
            assert_eq!(tag, "dis");
            assert_eq!(s, "\u{6e29}\u{5ea6}\u{4f20}\u{611f}\u{5668}");
        } else {
            panic!("expected Cmp(dis, Eq, Str), got: {:?}", result);
        }
    }

    #[test]
    fn test_backslash_escape_in_string() {
        // Escaped quote inside string: dis=="say \"hello\""
        let result = parse("dis==\"say \\\"hello\\\"\"");
        assert!(
            result.is_ok(),
            "escaped quotes should parse: {:?}",
            result.err()
        );
        if let Ok(Expr::Cmp(_, CmpOp::Eq, Value::Str(s))) = &result {
            assert_eq!(s, "say \"hello\"");
        } else {
            panic!("expected string with escaped quotes, got: {:?}", result);
        }
    }

    #[test]
    fn test_backslash_in_string_value() {
        // Backslash followed by non-special char: dis=="C:\\path"
        let result = parse("dis==\"C:\\\\path\"");
        assert!(
            result.is_ok(),
            "backslash in string should parse: {:?}",
            result.err()
        );
        if let Ok(Expr::Cmp(_, CmpOp::Eq, Value::Str(s))) = &result {
            assert_eq!(s, "C:\\path");
        } else {
            panic!("expected string with backslash, got: {:?}", result);
        }
    }

    #[test]
    fn test_numeric_edge_cases() {
        // Very large number
        let result = parse("cur > 999999999");
        assert!(result.is_ok(), "large number should parse");
        if let Ok(Expr::Cmp(_, CmpOp::Gt, Value::Num(n))) = &result {
            assert!((*n - 999999999.0).abs() < f64::EPSILON);
        }

        // Negative number
        let result = parse("cur > -100");
        assert!(
            result.is_ok(),
            "negative number should parse: {:?}",
            result.err()
        );
        if let Ok(Expr::Cmp(_, CmpOp::Gt, Value::Num(n))) = &result {
            assert!((*n - (-100.0)).abs() < f64::EPSILON);
        }

        // Zero
        let result = parse("cur == 0");
        assert!(result.is_ok(), "zero should parse");

        // Fractional number
        let result = parse("cur < 3.14159");
        assert!(result.is_ok(), "decimal should parse");
        if let Ok(Expr::Cmp(_, CmpOp::Lt, Value::Num(n))) = &result {
            assert!((*n - 3.14159).abs() < 0.0001);
        }
    }

    #[test]
    fn test_all_comparison_operators() {
        let ch = test_channel(100, "Sensor", "Analog", "In");
        // channel id = 100, cur = 72.5

        // == (already tested, but test with cur)
        assert!(matches(&parse("cur == 72.5").unwrap(), &ch));
        assert!(!matches(&parse("cur == 99.0").unwrap(), &ch));

        // !=
        assert!(matches(&parse("cur != 99.0").unwrap(), &ch));
        assert!(!matches(&parse("cur != 72.5").unwrap(), &ch));

        // <
        assert!(matches(&parse("cur < 80").unwrap(), &ch));
        assert!(!matches(&parse("cur < 70").unwrap(), &ch));

        // <=
        assert!(matches(&parse("cur <= 72.5").unwrap(), &ch));
        assert!(matches(&parse("cur <= 80").unwrap(), &ch));
        assert!(!matches(&parse("cur <= 70").unwrap(), &ch));

        // >
        assert!(matches(&parse("cur > 70").unwrap(), &ch));
        assert!(!matches(&parse("cur > 80").unwrap(), &ch));

        // >=
        assert!(matches(&parse("cur >= 72.5").unwrap(), &ch));
        assert!(matches(&parse("cur >= 70").unwrap(), &ch));
        assert!(!matches(&parse("cur >= 80").unwrap(), &ch));
    }

    #[test]
    fn test_malformed_filter_missing_operand() {
        // Trailing operator with no value
        let result = parse("channel ==");
        assert!(result.is_err(), "missing operand after == should fail");

        // Double operator
        let result = parse("channel == ==");
        assert!(result.is_err(), "double operator should fail");

        // Just an operator
        let result = parse("==");
        assert!(result.is_err(), "bare operator should fail");

        // Incomplete AND
        let result = parse("point and");
        assert!(result.is_err(), "trailing 'and' should fail");

        // Incomplete OR
        let result = parse("point or");
        assert!(result.is_err(), "trailing 'or' should fail");

        // Unclosed parenthesis
        let result = parse("(point and analog");
        assert!(result.is_err(), "unclosed paren should fail");

        // Unmatched close paren at start
        let result = parse(")point");
        assert!(result.is_err(), "leading close paren should fail");
    }

    #[test]
    fn test_filter_channel_eq_matching() {
        let ch = test_channel(1113, "AI1", "Analog", "In");
        // channel==1113 should match
        assert!(matches(&parse("channel==1113").unwrap(), &ch));
        // channel==612 should not match
        assert!(!matches(&parse("channel==612").unwrap(), &ch));
        // channel comparison operators
        assert!(matches(&parse("channel >= 1113").unwrap(), &ch));
        assert!(matches(&parse("channel <= 1113").unwrap(), &ch));
        assert!(!matches(&parse("channel > 1113").unwrap(), &ch));
        assert!(!matches(&parse("channel < 1113").unwrap(), &ch));
    }

    #[test]
    fn test_eval_boolean_comparison() {
        let ch_enabled = test_channel(1, "A", "Analog", "In");
        assert!(matches(&parse("enabled == true").unwrap(), &ch_enabled));
        assert!(!matches(&parse("enabled == false").unwrap(), &ch_enabled));
        assert!(matches(&parse("enabled != false").unwrap(), &ch_enabled));

        let mut ch_disabled = test_channel(2, "B", "Analog", "In");
        ch_disabled.enabled = false;
        assert!(matches(&parse("enabled == false").unwrap(), &ch_disabled));
        assert!(!matches(&parse("enabled == true").unwrap(), &ch_disabled));
    }

    // ═══════════════════════════════════════════════════════════
    // Dynamic tag filter tests (matches_with_tags)
    // ═══════════════════════════════════════════════════════════

    fn make_tags(entries: &[(&str, DynValue)]) -> HashMap<String, DynValue> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn test_dyn_tag_int_eq_match() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("modbusAddr", DynValue::Int(40001))]);
        let expr = parse("modbusAddr==40001").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_int_eq_no_match() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("modbusAddr", DynValue::Int(40001))]);
        let expr = parse("modbusAddr==40002").unwrap();
        assert!(!matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_missing_tag_no_match() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("modbusAddr", DynValue::Int(40001))]);
        let expr = parse("bacnetObj==100").unwrap();
        assert!(!matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_marker_presence() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("sensor", DynValue::Marker)]);
        let expr = parse("sensor").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_marker_absent() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("sensor", DynValue::Marker)]);
        let expr = parse("cmd").unwrap();
        // "cmd" is not in static fields or dynamic tags — should match on
        // has_tag label substring. Since "AI1" doesn't contain "cmd", this
        // should be false.
        assert!(!matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_string_eq() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("dis", DynValue::Str("Zone Temperature".into()))]);
        let expr = parse("dis==\"Zone Temperature\"").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_float_comparison() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("modbusScale", DynValue::Float(0.01))]);
        let expr = parse("modbusScale < 1").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_bool_eq() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("writable", DynValue::Bool(true))]);
        let expr = parse("writable == true").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));
        let expr2 = parse("writable == false").unwrap();
        assert!(!matches_with_tags(&expr2, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_conjunction() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[
            ("modbusAddr", DynValue::Int(40001)),
            ("dis", DynValue::Str("Zone Temperature".into())),
        ]);
        let expr = parse("modbusAddr==40001 and dis==\"Zone Temperature\"").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));

        let expr2 = parse("modbusAddr==40001 and dis==\"Outside Air\"").unwrap();
        assert!(!matches_with_tags(&expr2, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_disjunction() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("dis", DynValue::Str("Zone Temperature".into()))]);
        let expr = parse("dis==\"Zone Temperature\" or dis==\"Outside Air\"").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_null_not_present() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("nullTag", DynValue::Null)]);
        // Null tag should NOT be treated as present for marker check
        let expr = parse("nullTag").unwrap();
        assert!(!matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_mixed_static_and_dynamic() {
        let ch = test_channel(1113, "AI1 Thermistor", "Analog", "In");
        let tags = make_tags(&[("modbusAddr", DynValue::Int(40001))]);
        // Static property "analog" should still match, combined with dynamic tag
        let expr = parse("analog and modbusAddr==40001").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_none_tags_fallback() {
        let ch = test_channel(1113, "AI1", "Analog", "In");
        // When tags are None, should fall back to static matching only
        let expr = parse("analog").unwrap();
        assert!(matches_with_tags(&expr, &ch, None));
        let expr2 = parse("modbusAddr==40001").unwrap();
        assert!(!matches_with_tags(&expr2, &ch, None));
    }

    #[test]
    fn test_dyn_tag_ref_string_match() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("siteRef", DynValue::Ref("@site1".into()))]);
        let expr = parse("siteRef==\"@site1\"").unwrap();
        assert!(matches_with_tags(&expr, &ch, Some(&tags)));
    }

    #[test]
    fn test_dyn_tag_not_missing() {
        let ch = test_channel(100, "AI1", "Analog", "In");
        let tags = make_tags(&[("sensor", DynValue::Marker)]);
        // "not sensor" should be false when sensor is present
        let expr = parse("not sensor").unwrap();
        assert!(!matches_with_tags(&expr, &ch, Some(&tags)));
    }
}
