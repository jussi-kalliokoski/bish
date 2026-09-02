// Small recursive-descent integer arithmetic evaluator for $((...)),
// ((...)), and `let`. Standalone from the shell's own lexer/parser --
// arithmetic has a completely different token set (numbers, C-like
// operators) so reusing the shell tokenizer would just add indirection.

pub trait VarContext {
    fn get(&mut self, name: &str) -> i64;
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
    Comma,
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
            if c == '0' && i + 1 < chars.len() && (chars[i + 1] == 'x' || chars[i + 1] == 'X') {
                i += 2;
                let hstart = i;
                while i < chars.len() && chars[i].is_ascii_hexdigit() {
                    i += 1;
                }
                let n = i64::from_str_radix(&chars[hstart..i].iter().collect::<String>(), 16)
                    .map_err(|_| "bad hex number in arithmetic expression".to_string())?;
                toks.push(Tok::Num(n));
                continue;
            }
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            // base#digits (e.g. 16#FF, 2#101) -- the base is what was just
            // scanned as plain decimal digits.
            if i < chars.len() && chars[i] == '#' {
                let base: u32 = chars[start..i].iter().collect::<String>().parse().unwrap_or(10);
                i += 1;
                let dstart = i;
                while i < chars.len() && chars[i].is_alphanumeric() {
                    i += 1;
                }
                let digits: String = chars[dstart..i].iter().collect();
                let n = i64::from_str_radix(&digits, base).map_err(|_| format!("bad base-{} number in arithmetic expression", base))?;
                toks.push(Tok::Num(n));
                continue;
            }
            let digits: String = chars[start..i].iter().collect();
            // Leading-zero octal (bash: 010 is 8, not 10); a bare "0" (or
            // "00") still just parses as 0 either way.
            let n: i64 = if digits.len() > 1 && digits.starts_with('0') {
                i64::from_str_radix(&digits, 8).map_err(|_| "bad octal number in arithmetic expression".to_string())?
            } else {
                digits.parse().map_err(|_| "bad number in arithmetic expression".to_string())?
            };
            toks.push(Tok::Num(n));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            // `a[1]`, `a[i+1]`, `m[key]`: the subscript belongs to the
            // name, brackets and all -- `$((a[1]))` is an ordinary way
            // to read an element, and the VarContext is what knows how
            // to resolve one. Nesting is counted so `a[b[0]]` works.
            if i < chars.len() && chars[i] == '[' {
                let mut depth = 0;
                while i < chars.len() {
                    match chars[i] {
                        '[' => depth += 1,
                        ']' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            toks.push(Tok::Ident(chars[start..i].iter().collect()));
            continue;
        }
        if c == '$' {
            i += 1;
            if i < chars.len() && chars[i] == '{' {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '}' {
                    i += 1;
                }
                toks.push(Tok::Ident(chars[start..i].iter().collect()));
                if i < chars.len() {
                    i += 1;
                }
                continue;
            }
            if i < chars.len() && chars[i].is_ascii_digit() {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                toks.push(Tok::Ident(chars[start..i].iter().collect()));
                continue;
            }
            if i < chars.len() && (chars[i].is_alphabetic() || chars[i] == '_') {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                toks.push(Tok::Ident(chars[start..i].iter().collect()));
                continue;
            }
            if i < chars.len() && "#?@*$!".contains(chars[i]) {
                toks.push(Tok::Ident(chars[i].to_string()));
                i += 1;
                continue;
            }
            return Err("bad substitution in arithmetic expression".to_string());
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
        if c == ',' {
            toks.push(Tok::Comma);
            i += 1;
            continue;
        }
        let three: String = chars[i..(i + 3).min(chars.len())].iter().collect();
        const THREE_CHAR_OPS: [&str; 2] = ["<<=", ">>="];
        if THREE_CHAR_OPS.contains(&three.as_str()) {
            toks.push(Tok::Op(three));
            i += 3;
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        const TWO_CHAR_OPS: [&str; 19] =
            ["==", "!=", "<=", ">=", "&&", "||", "<<", ">>", "++", "--", "**", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^="];
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
        self.parse_comma()
    }

    // The comma operator (`a=1, b=2, a+b`): evaluates every comma-
    // separated subexpression left to right for its side effects,
    // keeping only the last one's value -- bash's own semantics, valid
    // both at the top level of ((...))/$((...)) and inside a
    // parenthesized grouping (see parse_primary's Tok::LParen arm,
    // which calls this same level rather than parse_assign directly).
    fn parse_comma(&mut self) -> Result<i64, String> {
        let mut v = self.parse_assign()?;
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            v = self.parse_assign()?;
        }
        Ok(v)
    }

    fn parse_assign(&mut self) -> Result<i64, String> {
        if let Tok::Ident(name) = self.peek().clone() {
            if let Some(Tok::Op(op)) = self.toks.get(self.pos + 1).cloned() {
                if op == "=" {
                    self.pos += 2;
                    let val = self.parse_assign()?;
                    self.ctx.set(&name, val);
                    return Ok(val);
                }
                if matches!(op.as_str(), "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "|=" | "^=") {
                    self.pos += 2;
                    let rhs = self.parse_assign()?;
                    let cur = self.ctx.get(&name);
                    let new_val = match op.as_str() {
                        "+=" => cur.wrapping_add(rhs),
                        "-=" => cur.wrapping_sub(rhs),
                        "*=" => cur.wrapping_mul(rhs),
                        "/=" => {
                            if rhs == 0 {
                                return Err("division by zero".to_string());
                            }
                            cur / rhs
                        }
                        "%=" => {
                            if rhs == 0 {
                                return Err("division by zero".to_string());
                            }
                            cur % rhs
                        }
                        "<<=" => cur << rhs,
                        ">>=" => cur >> rhs,
                        "&=" => cur & rhs,
                        "|=" => cur | rhs,
                        "^=" => cur ^ rhs,
                        _ => unreachable!(),
                    };
                    self.ctx.set(&name, new_val);
                    return Ok(new_val);
                }
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
        let mut v = self.parse_pow()?;
        loop {
            if self.peek_op("*") {
                self.advance();
                let r = self.parse_pow()?;
                v = v.wrapping_mul(r);
            } else if self.peek_op("/") {
                self.advance();
                let r = self.parse_pow()?;
                if r == 0 {
                    return Err("division by zero".to_string());
                }
                v /= r;
            } else if self.peek_op("%") {
                self.advance();
                let r = self.parse_pow()?;
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

    // `**` binds tighter than `* / %` but looser than unary +/-/!/~, and is
    // right-associative (2**3**2 is 2**(3**2) = 512, not (2**3)**2 = 64) --
    // matching bash's precedence and associativity exactly.
    fn parse_pow(&mut self) -> Result<i64, String> {
        let base = self.parse_unary()?;
        if self.peek_op("**") {
            self.advance();
            let exp = self.parse_pow()?;
            if exp < 0 {
                return Err("exponent less than 0".to_string());
            }
            return Ok(ipow(base, exp as u64));
        }
        Ok(base)
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
                let v = self.parse_comma()?;
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

// Wrapping integer exponentiation (matches how +/-/* already wrap here
// rather than panicking on overflow, since bash's arithmetic is
// fixed-width and just wraps).
fn ipow(base: i64, mut exp: u64) -> i64 {
    let mut result: i64 = 1;
    let mut b = base;
    while exp > 0 {
        if exp & 1 == 1 {
            result = result.wrapping_mul(b);
        }
        b = b.wrapping_mul(b);
        exp >>= 1;
    }
    result
}

pub fn eval(src: &str, ctx: &mut dyn VarContext) -> Result<i64, String> {
    let toks = tokenize(src)?;
    let mut p = Parser { toks, pos: 0, ctx };
    p.parse_expr()
}
