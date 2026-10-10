// Integer arithmetic for `$((...))`, `((...))` and `let`. It has its own
// tokenizer: the token set has nothing in common with the shell's.

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
            // The whole run of digit-set characters is one literal, checked
            // against the base afterwards, so `12a` and `0b101` are errors
            // as in bash.
            let digit_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '@';
            // `base#digits`, e.g. `16#FF`.
            let mut scan = i;
            while scan < chars.len() && chars[scan].is_ascii_digit() {
                scan += 1;
            }
            if scan < chars.len() && chars[scan] == '#' {
                let base: u32 = chars[start..scan].iter().collect::<String>().parse().unwrap_or(10);
                i = scan + 1;
                let dstart = i;
                while i < chars.len() && digit_char(chars[i]) {
                    i += 1;
                }
                let digits: String = chars[dstart..i].iter().collect();
                let n = base_number(base, &digits, &format!("{}#{}", base, digits))?;
                toks.push(Tok::Num(n));
                continue;
            }
            if c == '0' && i + 1 < chars.len() && (chars[i + 1] == 'x' || chars[i + 1] == 'X') {
                i += 2;
                let hstart = i;
                while i < chars.len() && digit_char(chars[i]) {
                    i += 1;
                }
                let digits: String = chars[hstart..i].iter().collect();
                let token: String = chars[start..i].iter().collect();
                // `0x` alone is 0, but `64#` alone is an error, as in bash.
                let n = if digits.is_empty() { 0 } else { base_number(16, &digits, &token)? };
                toks.push(Tok::Num(n));
                continue;
            }
            while i < chars.len() && digit_char(chars[i]) {
                i += 1;
            }
            let digits: String = chars[start..i].iter().collect();
            // A leading zero means octal: 010 is 8.
            let base = if digits.len() > 1 && digits.starts_with('0') { 8 } else { 10 };
            let n = base_number(base, &digits, &digits)?;
            toks.push(Tok::Num(n));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            // A subscript is part of the name (`a[i+1]`, `m[key]`), for the
            // VarContext to resolve. Nesting is counted so `a[b[0]]` works.
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
        return Err(format!("arithmetic syntax error: operand expected (error token is \"{}\")", c));
    }
    toks.push(Tok::Eof);
    Ok(toks)
}

impl Tok {
    /// The token as it was written, for an error that has to name it.
    fn text(&self) -> String {
        match self {
            Tok::Num(n) => n.to_string(),
            Tok::Ident(name) => name.clone(),
            Tok::Op(op) => op.clone(),
            Tok::LParen => "(".to_string(),
            Tok::RParen => ")".to_string(),
            Tok::Question => "?".to_string(),
            Tok::Colon => ":".to_string(),
            Tok::Comma => ",".to_string(),
            Tok::Eof => "end of expression".to_string(),
        }
    }
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

    /// The token last consumed when parsing failed: the one bash names in
    /// `(error token is "...")`. `1/0` names `0`, `1+` names `+`.
    fn error_token(&self) -> String {
        self.toks.get(self.pos.saturating_sub(1)).map(Tok::text).unwrap_or_default()
    }

    fn fail(&self, message: &str) -> String {
        format!("{} (error token is \"{}\")", message, self.error_token())
    }

    fn peek_op(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if o == s)
    }

    fn parse_expr(&mut self) -> Result<i64, String> {
        self.parse_comma()
    }

    // The comma operator: every subexpression for its side effects, the
    // last one for the value.
    fn parse_comma(&mut self) -> Result<i64, String> {
        // Every `( )` comes back through here, so this one check bounds
        // the nesting for parsing and evaluation alike.
        if crate::stackguard::deeply_exhausted() {
            return Err("expression nested too deeply".to_string());
        }
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
                                return Err(self.fail("division by 0"));
                            }
                            cur / rhs
                        }
                        "%=" => {
                            if rhs == 0 {
                                return Err(self.fail("division by 0"));
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
                _ => return Err(self.fail("`:' expected for conditional expression")),
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
                    return Err(self.fail("division by 0"));
                }
                v /= r;
            } else if self.peek_op("%") {
                self.advance();
                let r = self.parse_pow()?;
                if r == 0 {
                    return Err(self.fail("division by 0"));
                }
                v %= r;
            } else {
                break;
            }
        }
        Ok(v)
    }

    // Binds tighter than `* / %` and looser than unary operators, and is
    // right-associative: 2**3**2 is 512.
    fn parse_pow(&mut self) -> Result<i64, String> {
        let base = self.parse_unary()?;
        if self.peek_op("**") {
            self.advance();
            let exp = self.parse_pow()?;
            if exp < 0 {
                return Err(self.fail("exponent less than 0"));
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
            if let Tok::Ident(name) = self.peek().clone() {
                self.advance();
                let v = self.ctx.get(&name) + 1;
                self.ctx.set(&name, v);
                return Ok(v);
            }
            // bash reads `++5` as `+(+5)`; `++` is only an operator before
            // a name.
            return self.parse_unary();
        }
        if self.peek_op("--") {
            self.advance();
            if let Tok::Ident(name) = self.peek().clone() {
                self.advance();
                let v = self.ctx.get(&name) - 1;
                self.ctx.set(&name, v);
                return Ok(v);
            }
            // `--5` is `-(-5)`, for the same reason.
            let v = self.parse_unary()?;
            return Ok(v);
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
                    _ => return Err(self.fail("arithmetic syntax error in expression")),
                }
                Ok(v)
            }
            _ => Err(self.fail("arithmetic syntax error: operand expected")),
        }
    }
}

// Wraps on overflow, like the other operators here and like bash.
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

// bash's digit set, up to base 64: 0-9, a-z, A-Z, `@`, `_`. Up to base
// 36 the letters fold case, so `16#FF` and `16#ff` are both 255. Not
// `from_str_radix`, which panics above base 36.
fn base_number(base: u32, digits: &str, token: &str) -> Result<i64, String> {
    if base == 0 {
        return Err(format!("{}: invalid number", token));
    }
    if base == 1 || base > 64 {
        return Err(format!("{}: invalid arithmetic base", token));
    }
    if digits.is_empty() {
        return Err(format!("{}: invalid integer constant", token));
    }
    let mut value: i64 = 0;
    for c in digits.chars() {
        let d = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 10,
            'A'..='Z' if base <= 36 => c as u32 - 'A' as u32 + 10,
            'A'..='Z' => c as u32 - 'A' as u32 + 36,
            '@' => 62,
            '_' => 63,
            _ => return Err(format!("{}: invalid integer constant", token)),
        };
        if d >= base {
            return Err(format!("{}: value too great for base", token));
        }
        value = value.wrapping_mul(base as i64).wrapping_add(d as i64);
    }
    Ok(value)
}

pub fn eval(src: &str, ctx: &mut dyn VarContext) -> Result<i64, String> {
    let toks = tokenize(src)?;
    let mut p = Parser { toks, pos: 0, ctx };
    let value = p.parse_expr()?;
    // Leftover tokens are an error, as in bash, rather than a silently
    // half-read expression: `$((1 2))` isn't 1.
    if !matches!(p.peek(), Tok::Eof) {
        return Err(format!("arithmetic syntax error in expression (error token is \"{}\")", p.peek().text()));
    }
    Ok(value)
}
