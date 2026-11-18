//! Integer expression parser with parentheses and precedence.

const MAX_EXPRESSION_DEPTH: usize = 128;

/// Expression parse or arithmetic failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExpressionError {
    /// Input is empty or malformed.
    #[error("invalid expression")]
    Invalid,
    /// Division by zero.
    #[error("division by zero")]
    DivideByZero,
    /// Integer overflow.
    #[error("numeric overflow")]
    Overflow,
}

#[derive(Clone, Copy)]
enum Token {
    Number(i64),
    Operator(char),
    Left,
    Right,
}

/// Evaluate `+`, `-`, `*`, `/`, parentheses and unary signs over `i64`.
pub fn evaluate(input: &str) -> Result<i64, ExpressionError> {
    let tokens = tokenize(input)?;
    let mut parser = Parser {
        tokens,
        position: 0,
        depth: 0,
    };
    let value = parser.expression()?;
    if parser.position == parser.tokens.len() {
        Ok(value)
    } else {
        Err(ExpressionError::Invalid)
    }
}

fn tokenize(input: &str) -> Result<Vec<Token>, ExpressionError> {
    let mut tokens = Vec::new();
    let mut digits = String::new();
    for character in input.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        if !digits.is_empty() {
            let value = digits
                .parse::<i64>()
                .map_err(|_| ExpressionError::Overflow)?;
            tokens.push(Token::Number(value));
            digits.clear();
        }
        match character {
            ' ' => {}
            '+' | '-' | '*' | '/' => tokens.push(Token::Operator(character)),
            '(' => tokens.push(Token::Left),
            ')' => tokens.push(Token::Right),
            _ => return Err(ExpressionError::Invalid),
        }
    }
    if !digits.is_empty() {
        let value = digits
            .parse::<i64>()
            .map_err(|_| ExpressionError::Overflow)?;
        tokens.push(Token::Number(value));
    }
    if tokens.is_empty() {
        return Err(ExpressionError::Invalid);
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.position).copied()
    }

    fn take(&mut self) -> Option<Token> {
        let token = self.peek()?;
        self.position += 1;
        Some(token)
    }

    fn expression(&mut self) -> Result<i64, ExpressionError> {
        let mut value = self.term()?;
        while let Some(Token::Operator(operator @ ('+' | '-'))) = self.peek() {
            self.take();
            let right = self.term()?;
            value = if operator == '+' {
                value.checked_add(right)
            } else {
                value.checked_sub(right)
            }
            .ok_or(ExpressionError::Overflow)?;
        }
        Ok(value)
    }

    fn term(&mut self) -> Result<i64, ExpressionError> {
        let mut value = self.factor()?;
        while let Some(Token::Operator(operator @ ('*' | '/'))) = self.peek() {
            self.take();
            let right = self.factor()?;
            value = if operator == '*' {
                value.checked_mul(right)
            } else {
                if right == 0 {
                    return Err(ExpressionError::DivideByZero);
                }
                value.checked_div(right)
            }
            .ok_or(ExpressionError::Overflow)?;
        }
        Ok(value)
    }

    fn factor(&mut self) -> Result<i64, ExpressionError> {
        if self.depth >= MAX_EXPRESSION_DEPTH {
            return Err(ExpressionError::Invalid);
        }
        self.depth += 1;
        let result = self.factor_inner();
        self.depth -= 1;
        result
    }

    fn factor_inner(&mut self) -> Result<i64, ExpressionError> {
        match self.take().ok_or(ExpressionError::Invalid)? {
            Token::Number(value) => Ok(value),
            Token::Operator('-') => self
                .factor()?
                .checked_neg()
                .ok_or(ExpressionError::Overflow),
            Token::Operator('+') => self.factor(),
            Token::Left => {
                let value = self.expression()?;
                match self.take() {
                    Some(Token::Right) => Ok(value),
                    _ => Err(ExpressionError::Invalid),
                }
            }
            _ => Err(ExpressionError::Invalid),
        }
    }
}

#[derive(Clone, Copy)]
enum DecimalToken {
    Number(f64),
    Operator(char),
    Left,
    Right,
}

/// Evaluate the same grammar over `f64` decimals.
pub fn evaluate_decimal(input: &str) -> Result<f64, ExpressionError> {
    let tokens = tokenize_decimal(input)?;
    let mut parser = DecimalParser {
        tokens,
        position: 0,
        depth: 0,
    };
    let value = parser.expression()?;
    if parser.position != parser.tokens.len() || !value.is_finite() {
        return Err(ExpressionError::Invalid);
    }
    Ok(value)
}

/// Format a decimal result without trailing zeroes.
#[must_use]
pub fn format_decimal(value: f64) -> String {
    let text = format!("{value:.10}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "-0" || trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn tokenize_decimal(input: &str) -> Result<Vec<DecimalToken>, ExpressionError> {
    let mut tokens = Vec::new();
    let mut number = String::new();
    let mut dot = false;
    for character in input.chars() {
        if character.is_ascii_digit() {
            number.push(character);
            continue;
        }
        if character == '.' && !dot {
            dot = true;
            number.push(character);
            continue;
        }
        if !number.is_empty() {
            let value = number
                .parse::<f64>()
                .map_err(|_| ExpressionError::Invalid)?;
            value
                .is_finite()
                .then_some(())
                .ok_or(ExpressionError::Overflow)?;
            tokens.push(DecimalToken::Number(value));
            number.clear();
            dot = false;
        }
        match character {
            ' ' => {}
            '+' | '-' | '*' | '/' => tokens.push(DecimalToken::Operator(character)),
            '(' => tokens.push(DecimalToken::Left),
            ')' => tokens.push(DecimalToken::Right),
            _ => return Err(ExpressionError::Invalid),
        }
    }
    if !number.is_empty() {
        let value = number
            .parse::<f64>()
            .map_err(|_| ExpressionError::Invalid)?;
        if !value.is_finite() {
            return Err(ExpressionError::Overflow);
        }
        tokens.push(DecimalToken::Number(value));
    }
    if tokens.is_empty() {
        return Err(ExpressionError::Invalid);
    }
    Ok(tokens)
}

struct DecimalParser {
    tokens: Vec<DecimalToken>,
    position: usize,
    depth: usize,
}

impl DecimalParser {
    fn peek(&self) -> Option<DecimalToken> {
        self.tokens.get(self.position).copied()
    }

    fn take(&mut self) -> Option<DecimalToken> {
        let token = self.peek()?;
        self.position += 1;
        Some(token)
    }

    fn expression(&mut self) -> Result<f64, ExpressionError> {
        let mut value = self.term()?;
        while let Some(DecimalToken::Operator(operator @ ('+' | '-'))) = self.peek() {
            self.take();
            let right = self.term()?;
            value = if operator == '+' {
                value + right
            } else {
                value - right
            };
        }
        Ok(value)
    }

    fn term(&mut self) -> Result<f64, ExpressionError> {
        let mut value = self.factor()?;
        while let Some(DecimalToken::Operator(operator @ ('*' | '/'))) = self.peek() {
            self.take();
            let right = self.factor()?;
            value = if operator == '*' {
                value * right
            } else {
                if right == 0.0 {
                    return Err(ExpressionError::DivideByZero);
                }
                value / right
            };
        }
        Ok(value)
    }

    fn factor(&mut self) -> Result<f64, ExpressionError> {
        if self.depth >= MAX_EXPRESSION_DEPTH {
            return Err(ExpressionError::Invalid);
        }
        self.depth += 1;
        let result = self.factor_inner();
        self.depth -= 1;
        result
    }

    fn factor_inner(&mut self) -> Result<f64, ExpressionError> {
        match self.take().ok_or(ExpressionError::Invalid)? {
            DecimalToken::Number(value) => Ok(value),
            DecimalToken::Operator('-') => Ok(-self.factor()?),
            DecimalToken::Operator('+') => self.factor(),
            DecimalToken::Left => {
                let value = self.expression()?;
                match self.take() {
                    Some(DecimalToken::Right) => Ok(value),
                    _ => Err(ExpressionError::Invalid),
                }
            }
            _ => Err(ExpressionError::Invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_finite_numeric_literal() {
        let literal = "9".repeat(400);
        assert!(matches!(evaluate(&literal), Err(ExpressionError::Overflow)));
        assert!(matches!(
            tokenize_decimal(&literal),
            Err(ExpressionError::Overflow)
        ));
    }

    #[test]
    fn applies_precedence_and_parentheses() {
        assert_eq!(evaluate("2+3*4").unwrap(), 14);
        assert_eq!(evaluate("(2+3)*4").unwrap(), 20);
        assert_eq!(evaluate("2*(3+(4-1))").unwrap(), 12);
        assert_eq!(evaluate("50/8").unwrap(), 6);
        assert_eq!(evaluate(" 2 + 3 ").unwrap(), 5);
    }

    #[test]
    fn handles_unary_signs() {
        assert_eq!(evaluate("-(2+3)").unwrap(), -5);
        assert_eq!(evaluate("--5").unwrap(), 5);
        assert_eq!(evaluate("+5").unwrap(), 5);
    }

    #[test]
    fn rejects_invalid_and_overflow() {
        assert_eq!(evaluate(""), Err(ExpressionError::Invalid));
        assert_eq!(evaluate("(2+3"), Err(ExpressionError::Invalid));
        assert_eq!(evaluate("2+"), Err(ExpressionError::Invalid));
        assert_eq!(evaluate("2a"), Err(ExpressionError::Invalid));
        assert_eq!(evaluate("   "), Err(ExpressionError::Invalid));
        assert_eq!(evaluate("(2+3)4"), Err(ExpressionError::Invalid));
        assert_eq!(evaluate("2**3"), Err(ExpressionError::Invalid));
        assert_eq!(
            evaluate("9223372036854775808"),
            Err(ExpressionError::Overflow)
        );
        assert_eq!(
            evaluate("9223372036854775807+1"),
            Err(ExpressionError::Overflow)
        );
    }

    #[test]
    fn error_messages_are_stable() {
        assert_eq!(evaluate("1/0").unwrap_err().to_string(), "division by zero");
        assert_eq!(
            evaluate("9223372036854775807+1").unwrap_err().to_string(),
            "numeric overflow"
        );
        assert_eq!(
            evaluate("2+").unwrap_err().to_string(),
            "invalid expression"
        );
    }

    #[test]
    fn evaluates_decimal_expressions() {
        assert_eq!(evaluate_decimal("1.5+2.25").unwrap(), 3.75);
        assert_eq!(evaluate_decimal("(1.5+2.5)*4").unwrap(), 16.0);
        assert_eq!(evaluate_decimal("5.5-2.5").unwrap(), 3.0);
        assert_eq!(evaluate_decimal("5.0/2.0").unwrap(), 2.5);
        assert_eq!(evaluate_decimal("1.5 + 2").unwrap(), 3.5);
        assert_eq!(evaluate_decimal("-1.5").unwrap(), -1.5);
        assert_eq!(evaluate_decimal("+1.5").unwrap(), 1.5);
        assert_eq!(format_decimal(16.0), "16");
        assert_eq!(format_decimal(3.75), "3.75");
        assert_eq!(format_decimal(-0.0), "0");
        assert_eq!(format_decimal(1.0 / 3.0), "0.3333333333");
    }

    #[test]
    fn rejects_invalid_decimals() {
        assert_eq!(evaluate_decimal("1.2.3"), Err(ExpressionError::Invalid));
        assert_eq!(
            evaluate_decimal("1/0.0"),
            Err(ExpressionError::DivideByZero)
        );
        assert_eq!(evaluate_decimal(""), Err(ExpressionError::Invalid));
        assert_eq!(evaluate_decimal("(1.5+2.5"), Err(ExpressionError::Invalid));
        assert_eq!(evaluate_decimal("1.0 2.0"), Err(ExpressionError::Invalid));
        assert_eq!(evaluate_decimal("2.0**3"), Err(ExpressionError::Invalid));
        let huge = format!("{}.0", "9".repeat(400));
        assert_eq!(evaluate_decimal(&huge), Err(ExpressionError::Overflow));
    }

    #[test]
    fn rejects_division_by_zero() {
        assert_eq!(evaluate("1/0"), Err(ExpressionError::DivideByZero));
        assert_eq!(evaluate("1/(2-2)"), Err(ExpressionError::DivideByZero));
    }

    #[test]
    fn deeply_nested_expression_is_rejected() {
        let deep = format!("{}1", "-".repeat(100_000));
        assert_eq!(evaluate(&deep), Err(ExpressionError::Invalid));
        let deep_decimal = format!("{}1.5", "-".repeat(100_000));
        assert_eq!(
            evaluate_decimal(&deep_decimal),
            Err(ExpressionError::Invalid)
        );
    }
}
