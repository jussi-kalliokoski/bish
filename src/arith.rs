// Small recursive-descent integer arithmetic evaluator for $((...)),
// ((...)), and `let`. Standalone from the shell's own lexer/parser --
// arithmetic has a completely different token set (numbers, C-like
// operators) so reusing the shell tokenizer would just add indirection.

pub trait VarContext {
    fn get(&self, name: &str) -> i64;
    fn set(&mut self, name: &str, value: i64);
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(i64),
    Ident(String),
    Op(String),
    LParen,
    RParen,
    Question,
    Colon,
    Eof,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let n: i64 = chars[start..i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| "bad number in arithmetic expression".to_string())?;
            toks.push(Tok::Num(n));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            toks.push(Tok::Ident(chars[start..i].iter().collect()));
            continue;
        }
        if c == '(' {
            toks.push(Tok::LParen);
            i += 1;
            continue;
        }
        if c == ')' {
            toks.push(Tok::RParen);
            i += 1;
            continue;
        }
        if c == '?' {
            toks.push(Tok::Question);
            i += 1;
            continue;
        }
        if c == ':' {
            toks.push(Tok::Colon);
            i += 1;
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        const TWO_CHAR_OPS: [&str; 10] =
            ["==", "!=", "<=", ">=", "&&", "||", "<<", ">>", "++", "--"];
        if TWO_CHAR_OPS.contains(&two.as_str()) {
            toks.push(Tok::Op(two));
            i += 2;
            continue;
        }
        if "+-*/%<>&|^!~=".contains(c) {
            toks.push(Tok::Op(c.to_string()));
            i += 1;
            continue;
        }
        return Err(format!("bad character in arithmetic expression: {:?}", c));
    }
    toks.push(Tok::Eof);
    Ok(toks)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    ctx: &'a mut dyn VarContext,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn peek_op(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if o == s)
    }

    fn parse_expr(&mut self) -> Result<i64, String> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<i64, String> {
        if let Tok::Ident(name) = self.peek().clone() {
            if self.toks.get(self.pos + 1) == Some(&Tok::Op("=".to_string())) {
                self.pos += 2;
                let val = self.parse_assign()?;
                self.ctx.set(&name, val);
                return Ok(val);
            }
        }
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> Result<i64, String> {
        let cond = self.parse_or()?;
        if matches!(self.peek(), Tok::Question) {
            self.advance();
            let a = self.parse_assign()?;
            match self.advance() {
                Tok::Colon => {}
                other => return Err(format!("expected ':', got {:?}", other)),
            }
            let b = self.parse_assign()?;
            Ok(if cond != 0 { a } else { b })
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<i64, String> {
        let mut v = self.parse_and()?;
        while self.peek_op("||") {
            self.advance();
            let r = self.parse_and()?;
            v = ((v != 0) || (r != 0)) as i64;
        }
        Ok(v)
    }

    fn parse_and(&mut self) -> Result<i64, String> {
        let mut v = self.parse_bitor()?;
        while self.peek_op("&&") {
            self.advance();
            let r = self.parse_bitor()?;
            v = ((v != 0) && (r != 0)) as i64;
        }
        Ok(v)
    }

    fn parse_bitor(&mut self) -> Result<i64, String> {
        let mut v = self.parse_bitxor()?;
        while self.peek_op("|") {
            self.advance();
            let r = self.parse_bitxor()?;
            v |= r;
        }
        Ok(v)
    }

    fn parse_bitxor(&mut self) -> Result<i64, String> {
        let mut v = self.parse_bitand()?;
        while self.peek_op("^") {
            self.advance();
            let r = self.parse_bitand()?;
            v ^= r;
        }
        Ok(v)
    }

    fn parse_bitand(&mut self) -> Result<i64, String> {
        let mut v = self.parse_eq()?;
        while self.peek_op("&") {
            self.advance();
            let r = self.parse_eq()?;
            v &= r;
        }
        Ok(v)
    }

    fn parse_eq(&mut self) -> Result<i64, String> {
        let mut v = self.parse_rel()?;
        loop {
            if self.peek_op("==") {
                self.advance();
                let r = self.parse_rel()?;
                v = (v == r) as i64;
            } else if self.peek_op("!=") {
                self.advance();
                let r = self.parse_rel()?;
                v = (v != r) as i64;
            } else {
                break;
            }
        }
        Ok(v)
    }

    fn parse_rel(&mut self) -> Result<i64, String> {
        let mut v = self.parse_shift()?;
        loop {
            if self.peek_op("<=") {
                self.advance();
                let r = self.parse_shift()?;
                v = (v <= r) as i64;
            } else if self.peek_op(">=") {
                self.advance();
                let r = self.parse_shift()?;
                v = (v >= r) as i64;
            } else if self.peek_op("<") {
                self.advance();
                let r = self.parse_shift()?;
                v = (v < r) as i64;
            } else if self.peek_op(">") {
                self.advance();
                let r = self.parse_shift()?;
                v = (v > r) as i64;
            } else {
                break;
            }
        }
        Ok(v)
    }

    fn parse_shift(&mut self) -> Result<i64, String> {
        let mut v = self.parse_add()?;
        loop {
            if self.peek_op("<<") {
                self.advance();
                let r = self.parse_add()?;
                v <<= r;
            } else if self.peek_op(">>") {
                self.advance();
                let r = self.parse_add()?;
                v >>= r;
            } else {
                break;
            }
        }
        Ok(v)
    }

    fn parse_add(&mut self) -> Result<i64, String> {
        let mut v = self.parse_mul()?;
        loop {
            if self.peek_op("+") {
                self.advance();
                let r = self.parse_mul()?;
                v = v.wrapping_add(r);
            } else if self.peek_op("-") {
                self.advance();
                let r = self.parse_mul()?;
                v = v.wrapping_sub(r);
            } else {
                break;
            }
        }
        Ok(v)
    }

    fn parse_mul(&mut self) -> Result<i64, String> {
        let mut v = self.parse_unary()?;
        loop {
            if self.peek_op("*") {
                self.advance();
                let r = self.parse_unary()?;
                v = v.wrapping_mul(r);
            } else if self.peek_op("/") {
                self.advance();
                let r = self.parse_unary()?;
                if r == 0 {
                    return Err("division by zero".to_string());
                }
                v /= r;
            } else if self.peek_op("%") {
                self.advance();
                let r = self.parse_unary()?;
                if r == 0 {
                    return Err("division by zero".to_string());
                }
                v %= r;
            } else {
                break;
            }
        }
        Ok(v)
    }

    fn parse_unary(&mut self) -> Result<i64, String> {
        if self.peek_op("!") {
            self.advance();
            let v = self.parse_unary()?;
            return Ok((v == 0) as i64);
        }
        if self.peek_op("~") {
            self.advance();
            let v = self.parse_unary()?;
            return Ok(!v);
        }
        if self.peek_op("-") {
            self.advance();
            let v = self.parse_unary()?;
            return Ok(-v);
        }
        if self.peek_op("+") {
            self.advance();
            return self.parse_unary();
        }
        if self.peek_op("++") {
            self.advance();
            if let Tok::Ident(name) = self.advance() {
                let v = self.ctx.get(&name) + 1;
                self.ctx.set(&name, v);
                return Ok(v);
            }
            return Err("'++' needs a variable".to_string());
        }
        if self.peek_op("--") {
            self.advance();
            if let Tok::Ident(name) = self.advance() {
                let v = self.ctx.get(&name) - 1;
                self.ctx.set(&name, v);
                return Ok(v);
            }
            return Err("'--' needs a variable".to_string());
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<i64, String> {
        match self.advance() {
            Tok::Num(n) => Ok(n),
            Tok::Ident(name) => {
                let old = self.ctx.get(&name);
                if self.peek_op("++") {
                    self.advance();
                    self.ctx.set(&name, old + 1);
                    Ok(old)
                } else if self.peek_op("--") {
                    self.advance();
                    self.ctx.set(&name, old - 1);
                    Ok(old)
                } else {
                    Ok(old)
                }
            }
            Tok::LParen => {
                let v = self.parse_assign()?;
                match self.advance() {
                    Tok::RParen => {}
                    other => return Err(format!("expected ')', got {:?}", other)),
                }
                Ok(v)
            }
            other => Err(format!("unexpected token in arithmetic expression: {:?}", other)),
        }
    }
}

pub fn eval(src: &str, ctx: &mut dyn VarContext) -> Result<i64, String> {
    let toks = tokenize(src)?;
    let mut p = Parser { toks, pos: 0, ctx };
    p.parse_expr()
}
