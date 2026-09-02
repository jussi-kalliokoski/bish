// The WHATWG HTML tokenizer (§13.2.5), hand-rolled -- no external crate,
// same as every other parser in this codebase. One state per state in
// the spec, named the same way, so a state here can be read straight
// against the section that defines it; the `//` comment on each arm is
// the spec's own wording where it isn't obvious.
//
// Written to the spec rather than to "what markdown files usually
// contain" on purpose: the whole reason to have a real tokenizer is the
// cases a reasonable-looking shortcut gets wrong -- `<a href=b?c=d>`
// (unquoted attribute values), `<!-->`, `&notit;` (a legacy reference
// that stops without its semicolon), `<script>` containing `</div>`,
// `a < b` in prose. Those all come out right here because the state
// machine is the spec's, not because any of them was special-cased.
//
// Not implemented, and it can't be: everything that only exists for a
// *scripting* host. There's no `document.write`, so the tokenizer never
// has to be re-entered mid-token, and the script-data double-escape
// states are still here (they change how `</script>` inside a string is
// tokenized) while actually running any of it is not a thing this can
// do. Character encoding detection is also out: input arrives as Rust
// `&str`, so it is already decoded UTF-8, which is what the spec's own
// "encoding sniffing" would have concluded for any modern document.

use super::entities;

// U+FFFD, which the spec substitutes for a NULL in most states rather
// than dropping it.
const REPLACEMENT: char = '\u{FFFD}';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub attrs: Vec<Attr>,
    pub self_closing: bool,
}

impl Tag {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.name == name).map(|a| a.value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doctype {
    pub name: Option<String>,
    pub public_id: Option<String>,
    pub system_id: Option<String>,
    pub force_quirks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Doctype(Doctype),
    StartTag(Tag),
    EndTag(Tag),
    Comment(String),
    // One character at a time, exactly as the spec emits them -- the
    // tree builder is what coalesces runs into text nodes, and it has
    // rules (whitespace-only runs in tables, for one) that depend on
    // seeing them individually.
    Char(char),
    Eof,
}

// Which of the four content models the *tree builder* has put the
// tokenizer into. The tokenizer can't decide this itself: whether
// `<title>` contents are markup depends on the element that opened, so
// the tree construction stage sets it (see Tokenizer::set_state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentState {
    Data,
    // `<title>`, `<textarea>`: character references still resolve, tags
    // don't -- except the matching end tag.
    Rcdata,
    // `<style>`, `<xmp>`, `<iframe>`, `<noembed>`, `<noframes>`: neither.
    Rawtext,
    ScriptData,
    Plaintext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Data,
    Rcdata,
    Rawtext,
    ScriptData,
    Plaintext,
    TagOpen,
    EndTagOpen,
    TagName,
    RcdataLessThanSign,
    RcdataEndTagOpen,
    RcdataEndTagName,
    RawtextLessThanSign,
    RawtextEndTagOpen,
    RawtextEndTagName,
    ScriptDataLessThanSign,
    ScriptDataEndTagOpen,
    ScriptDataEndTagName,
    ScriptDataEscapeStart,
    ScriptDataEscapeStartDash,
    ScriptDataEscaped,
    ScriptDataEscapedDash,
    ScriptDataEscapedDashDash,
    ScriptDataEscapedLessThanSign,
    ScriptDataEscapedEndTagOpen,
    ScriptDataEscapedEndTagName,
    ScriptDataDoubleEscapeStart,
    ScriptDataDoubleEscaped,
    ScriptDataDoubleEscapedDash,
    ScriptDataDoubleEscapedDashDash,
    ScriptDataDoubleEscapedLessThanSign,
    ScriptDataDoubleEscapeEnd,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDoubleQuoted,
    AttributeValueSingleQuoted,
    AttributeValueUnquoted,
    AfterAttributeValueQuoted,
    SelfClosingStartTag,
    BogusComment,
    MarkupDeclarationOpen,
    CommentStart,
    CommentStartDash,
    Comment,
    CommentLessThanSign,
    CommentLessThanSignBang,
    CommentLessThanSignBangDash,
    CommentLessThanSignBangDashDash,
    CommentEndDash,
    CommentEnd,
    CommentEndBang,
    Doctype,
    BeforeDoctypeName,
    DoctypeName,
    AfterDoctypeName,
    AfterDoctypePublicKeyword,
    BeforeDoctypePublicIdentifier,
    DoctypePublicIdentifierDoubleQuoted,
    DoctypePublicIdentifierSingleQuoted,
    AfterDoctypePublicIdentifier,
    BetweenDoctypePublicAndSystemIdentifiers,
    AfterDoctypeSystemKeyword,
    BeforeDoctypeSystemIdentifier,
    DoctypeSystemIdentifierDoubleQuoted,
    DoctypeSystemIdentifierSingleQuoted,
    AfterDoctypeSystemIdentifier,
    BogusDoctype,
    CdataSection,
    CdataSectionBracket,
    CdataSectionEnd,
    CharacterReference,
    NamedCharacterReference,
    AmbiguousAmpersand,
    NumericCharacterReference,
    HexadecimalCharacterReferenceStart,
    DecimalCharacterReferenceStart,
    HexadecimalCharacterReference,
    DecimalCharacterReference,
    NumericCharacterReferenceEnd,
}

pub struct Tokenizer {
    input: Vec<char>,
    pos: usize,
    state: State,
    // Where a character reference goes back to. The spec calls this the
    // "return state" and it's what makes one reference implementation
    // serve both text and attribute values.
    return_state: State,
    // Tokens produced by the current step. A single step can emit more
    // than one (a character reference expanding to two code points, a
    // run of text flushed before a tag), so this is drained by `next`.
    pending: Vec<Token>,
    tag: Tag,
    // Whether the tag being built is an end tag -- kept separate so the
    // one set of tag states serves both, as the spec does.
    end_tag: bool,
    attr: Option<Attr>,
    comment: String,
    doctype: Doctype,
    temp: String,
    char_ref_code: u32,
    // The last start tag name emitted, which is the only way an end tag
    // in RCDATA/RAWTEXT/script data can be recognized as "appropriate".
    last_start_tag: String,
    // Set by the tree builder when the adjusted current node is a
    // foreign (SVG/MathML) element -- the only context where `<![CDATA[`
    // is real markup rather than a bogus comment.
    cdata_ok: bool,
    // The parse errors the spec names. Collected rather than acted on:
    // HTML has no such thing as a fatal parse error, every one of them
    // has defined recovery, and something has to be able to *report*
    // them (see html::Document::errors) for a linter or a preview to say
    // "this markup is malformed" without changing what it renders.
    pub errors: Vec<String>,
}

impl Tokenizer {
    pub fn new(input: &str) -> Tokenizer {
        Tokenizer {
            // Pre-processing per §13.2.3.5: a CRLF pair and a lone CR
            // both become a single LF, before anything else looks at the
            // input.
            input: normalize_newlines(input),
            pos: 0,
            state: State::Data,
            return_state: State::Data,
            pending: Vec::new(),
            tag: Tag { name: String::new(), attrs: Vec::new(), self_closing: false },
            end_tag: false,
            attr: None,
            comment: String::new(),
            doctype: Doctype { name: None, public_id: None, system_id: None, force_quirks: false },
            temp: String::new(),
            char_ref_code: 0,
            last_start_tag: String::new(),
            cdata_ok: false,
            errors: Vec::new(),
        }
    }

    // The tree builder's own switch: `<title>` puts this in RCDATA,
    // `<script>` in script data, and so on. See ContentState.
    // See the `cdata_ok` field.
    pub fn set_cdata_ok(&mut self, ok: bool) {
        self.cdata_ok = ok;
    }

    pub fn set_state(&mut self, content: ContentState) {
        self.state = match content {
            ContentState::Data => State::Data,
            ContentState::Rcdata => State::Rcdata,
            ContentState::Rawtext => State::Rawtext,
            ContentState::ScriptData => State::ScriptData,
            ContentState::Plaintext => State::Plaintext,
        };
    }

    pub fn next(&mut self) -> Token {
        loop {
            if !self.pending.is_empty() {
                return self.pending.remove(0);
            }
            self.step();
        }
    }

    fn error(&mut self, what: &str) {
        self.errors.push(format!("{what} at position {}", self.pos));
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }

    // Advances past the end as well as within it, so that "reconsume"
    // is a plain decrement in every state including the EOF arms -- the
    // spec's own model, where EOF is a character you can put back.
    fn consume(&mut self) -> Option<char> {
        let c = self.input.get(self.pos).copied();
        self.pos = (self.pos + 1).min(self.input.len() + 1);
        c
    }

    fn reconsume(&mut self, state: State) {
        self.pos -= 1;
        self.state = state;
    }

    fn emit(&mut self, token: Token) {
        if let Token::StartTag(tag) = &token {
            self.last_start_tag = tag.name.clone();
        }
        self.pending.push(token);
    }

    fn emit_chars(&mut self, text: &str) {
        for c in text.chars() {
            self.pending.push(Token::Char(c));
        }
    }

    // Whether the end tag being built matches the last start tag emitted
    // -- §13.2.5's "appropriate end tag token", the condition that lets
    // `</script>` close a script while `</div>` inside one doesn't.
    fn appropriate_end_tag(&self) -> bool {
        self.end_tag && self.tag.name == self.last_start_tag
    }

    fn start_tag(&mut self, end: bool) {
        self.tag = Tag { name: String::new(), attrs: Vec::new(), self_closing: false };
        self.end_tag = end;
        self.attr = None;
    }

    fn finish_attr(&mut self) {
        let Some(attr) = self.attr.take() else { return };
        // "If there is already an attribute with that name, this is a
        // duplicate-attribute parse error and the new attribute is
        // removed" -- the *first* one wins.
        if self.tag.attrs.iter().any(|a| a.name == attr.name) {
            self.error("duplicate attribute");
            return;
        }
        self.tag.attrs.push(attr);
    }

    fn emit_tag(&mut self) {
        self.finish_attr();
        let tag = std::mem::replace(&mut self.tag, Tag { name: String::new(), attrs: Vec::new(), self_closing: false });
        if self.end_tag {
            // "An end tag with attributes / a self-closing end tag is a
            // parse error" -- reported, and the attributes ignored,
            // which is what the tree builder expects to receive.
            if !tag.attrs.is_empty() {
                self.error("end tag with attributes");
            }
            if tag.self_closing {
                self.error("self-closing end tag");
            }
            self.emit(Token::EndTag(tag));
        } else {
            self.emit(Token::StartTag(tag));
        }
    }

    // Whether the character reference being parsed sits in an attribute
    // value, which changes the rules for an unterminated named one.
    fn in_attribute(&self) -> bool {
        matches!(self.return_state, State::AttributeValueDoubleQuoted | State::AttributeValueSingleQuoted | State::AttributeValueUnquoted)
    }

    // §13.2.5.72's "flush code points consumed as a character
    // reference": into the attribute value if that's where we came from,
    // otherwise out as character tokens.
    fn flush_char_ref(&mut self) {
        let temp = std::mem::take(&mut self.temp);
        if self.in_attribute() {
            if let Some(attr) = self.attr.as_mut() {
                attr.value.push_str(&temp);
            }
        } else {
            self.emit_chars(&temp);
        }
    }

    fn step(&mut self) {
        match self.state {
            State::Data => match self.consume() {
                Some('&') => {
                    self.return_state = State::Data;
                    self.state = State::CharacterReference;
                }
                Some('<') => self.state = State::TagOpen,
                Some('\0') => {
                    // In data, a NULL is emitted as-is rather than
                    // replaced -- one of the few places that's true.
                    self.error("unexpected null character");
                    self.emit(Token::Char('\0'));
                }
                Some(c) => self.emit(Token::Char(c)),
                None => self.emit(Token::Eof),
            },
            State::Rcdata => match self.consume() {
                Some('&') => {
                    self.return_state = State::Rcdata;
                    self.state = State::CharacterReference;
                }
                Some('<') => self.state = State::RcdataLessThanSign,
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                }
                Some(c) => self.emit(Token::Char(c)),
                None => self.emit(Token::Eof),
            },
            State::Rawtext => match self.consume() {
                Some('<') => self.state = State::RawtextLessThanSign,
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                }
                Some(c) => self.emit(Token::Char(c)),
                None => self.emit(Token::Eof),
            },
            State::ScriptData => match self.consume() {
                Some('<') => self.state = State::ScriptDataLessThanSign,
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                }
                Some(c) => self.emit(Token::Char(c)),
                None => self.emit(Token::Eof),
            },
            State::Plaintext => match self.consume() {
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                }
                Some(c) => self.emit(Token::Char(c)),
                None => self.emit(Token::Eof),
            },
            State::TagOpen => match self.consume() {
                Some('!') => self.state = State::MarkupDeclarationOpen,
                Some('/') => self.state = State::EndTagOpen,
                Some(c) if c.is_ascii_alphabetic() => {
                    self.start_tag(false);
                    self.reconsume(State::TagName);
                }
                Some('?') => {
                    self.error("unexpected question mark instead of tag name");
                    self.comment.clear();
                    self.reconsume(State::BogusComment);
                }
                // `a < b`: a `<` that starts nothing is literal text.
                Some(_) => {
                    self.error("invalid first character of tag name");
                    self.emit(Token::Char('<'));
                    self.reconsume(State::Data);
                }
                None => {
                    self.error("eof before tag name");
                    self.emit(Token::Char('<'));
                    self.emit(Token::Eof);
                }
            },
            State::EndTagOpen => match self.consume() {
                Some(c) if c.is_ascii_alphabetic() => {
                    self.start_tag(true);
                    self.reconsume(State::TagName);
                }
                // `</>` is dropped entirely, not treated as text.
                Some('>') => {
                    self.error("missing end tag name");
                    self.state = State::Data;
                }
                Some(_) => {
                    self.error("invalid first character of tag name");
                    self.comment.clear();
                    self.reconsume(State::BogusComment);
                }
                None => {
                    self.error("eof before tag name");
                    self.emit_chars("</");
                    self.emit(Token::Eof);
                }
            },
            State::TagName => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => self.state = State::BeforeAttributeName,
                Some('/') => self.state = State::SelfClosingStartTag,
                Some('>') => {
                    self.state = State::Data;
                    self.emit_tag();
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.tag.name.push(REPLACEMENT);
                }
                Some(c) => self.tag.name.push(c.to_ascii_lowercase()),
                None => {
                    self.error("eof in tag");
                    self.emit(Token::Eof);
                }
            },
            // The three "less than sign" trios below are the same shape
            // three times over, differing only in which state they fall
            // back to -- the spec spells each out separately, and so
            // does this, because collapsing them would hide exactly the
            // differences that matter.
            State::RcdataLessThanSign => {
                if self.peek() == Some('/') {
                    self.pos += 1;
                    self.temp.clear();
                    self.state = State::RcdataEndTagOpen;
                } else {
                    self.emit(Token::Char('<'));
                    self.state = State::Rcdata;
                }
            }
            State::RcdataEndTagOpen => {
                if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.start_tag(true);
                    self.state = State::RcdataEndTagName;
                } else {
                    self.emit_chars("</");
                    self.state = State::Rcdata;
                }
            }
            State::RcdataEndTagName => self.end_tag_name(State::Rcdata),
            State::RawtextLessThanSign => {
                if self.peek() == Some('/') {
                    self.pos += 1;
                    self.temp.clear();
                    self.state = State::RawtextEndTagOpen;
                } else {
                    self.emit(Token::Char('<'));
                    self.state = State::Rawtext;
                }
            }
            State::RawtextEndTagOpen => {
                if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.start_tag(true);
                    self.state = State::RawtextEndTagName;
                } else {
                    self.emit_chars("</");
                    self.state = State::Rawtext;
                }
            }
            State::RawtextEndTagName => self.end_tag_name(State::Rawtext),
            State::ScriptDataLessThanSign => match self.peek() {
                Some('/') => {
                    self.pos += 1;
                    self.temp.clear();
                    self.state = State::ScriptDataEndTagOpen;
                }
                Some('!') => {
                    self.pos += 1;
                    self.emit_chars("<!");
                    self.state = State::ScriptDataEscapeStart;
                }
                _ => {
                    self.emit(Token::Char('<'));
                    self.state = State::ScriptData;
                }
            },
            State::ScriptDataEndTagOpen => {
                if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.start_tag(true);
                    self.state = State::ScriptDataEndTagName;
                } else {
                    self.emit_chars("</");
                    self.state = State::ScriptData;
                }
            }
            State::ScriptDataEndTagName => self.end_tag_name(State::ScriptData),
            // `<script><!--` puts the tokenizer in "escaped" script data,
            // where `</script>` still ends the element but the content
            // may contain `<`. The double-escaped states below handle a
            // `<script>` *inside* that comment, which is the case that
            // makes this worth implementing rather than approximating.
            State::ScriptDataEscapeStart => {
                if self.peek() == Some('-') {
                    self.pos += 1;
                    self.emit(Token::Char('-'));
                    self.state = State::ScriptDataEscapeStartDash;
                } else {
                    self.state = State::ScriptData;
                }
            }
            State::ScriptDataEscapeStartDash => {
                if self.peek() == Some('-') {
                    self.pos += 1;
                    self.emit(Token::Char('-'));
                    self.state = State::ScriptDataEscapedDashDash;
                } else {
                    self.state = State::ScriptData;
                }
            }
            State::ScriptDataEscaped => match self.consume() {
                Some('-') => {
                    self.emit(Token::Char('-'));
                    self.state = State::ScriptDataEscapedDash;
                }
                Some('<') => self.state = State::ScriptDataEscapedLessThanSign,
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                }
                Some(c) => self.emit(Token::Char(c)),
                None => {
                    self.error("eof in script html comment like text");
                    self.emit(Token::Eof);
                }
            },
            State::ScriptDataEscapedDash => match self.consume() {
                Some('-') => {
                    self.emit(Token::Char('-'));
                    self.state = State::ScriptDataEscapedDashDash;
                }
                Some('<') => self.state = State::ScriptDataEscapedLessThanSign,
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                    self.state = State::ScriptDataEscaped;
                }
                Some(c) => {
                    self.emit(Token::Char(c));
                    self.state = State::ScriptDataEscaped;
                }
                None => {
                    self.error("eof in script html comment like text");
                    self.emit(Token::Eof);
                }
            },
            State::ScriptDataEscapedDashDash => match self.consume() {
                Some('-') => self.emit(Token::Char('-')),
                Some('<') => self.state = State::ScriptDataEscapedLessThanSign,
                Some('>') => {
                    self.emit(Token::Char('>'));
                    self.state = State::ScriptData;
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                    self.state = State::ScriptDataEscaped;
                }
                Some(c) => {
                    self.emit(Token::Char(c));
                    self.state = State::ScriptDataEscaped;
                }
                None => {
                    self.error("eof in script html comment like text");
                    self.emit(Token::Eof);
                }
            },
            State::ScriptDataEscapedLessThanSign => match self.peek() {
                Some('/') => {
                    self.pos += 1;
                    self.temp.clear();
                    self.state = State::ScriptDataEscapedEndTagOpen;
                }
                Some(c) if c.is_ascii_alphabetic() => {
                    self.temp.clear();
                    self.emit(Token::Char('<'));
                    self.state = State::ScriptDataDoubleEscapeStart;
                }
                _ => {
                    self.emit(Token::Char('<'));
                    self.state = State::ScriptDataEscaped;
                }
            },
            State::ScriptDataEscapedEndTagOpen => {
                if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.start_tag(true);
                    self.state = State::ScriptDataEscapedEndTagName;
                } else {
                    self.emit_chars("</");
                    self.state = State::ScriptDataEscaped;
                }
            }
            State::ScriptDataEscapedEndTagName => self.end_tag_name(State::ScriptDataEscaped),
            State::ScriptDataDoubleEscapeStart => {
                let c = self.consume();
                match c {
                    Some('\t') | Some('\n') | Some('\x0C') | Some(' ') | Some('/') | Some('>') => {
                        let temp = self.temp.clone();
                        self.state = if temp == "script" { State::ScriptDataDoubleEscaped } else { State::ScriptDataEscaped };
                        self.emit(Token::Char(c.unwrap()));
                    }
                    Some(c) if c.is_ascii_alphabetic() => {
                        self.temp.push(c.to_ascii_lowercase());
                        self.emit(Token::Char(c));
                    }
                    _ => self.reconsume(State::ScriptDataEscaped),
                }
            }
            State::ScriptDataDoubleEscaped => match self.consume() {
                Some('-') => {
                    self.emit(Token::Char('-'));
                    self.state = State::ScriptDataDoubleEscapedDash;
                }
                Some('<') => {
                    self.emit(Token::Char('<'));
                    self.state = State::ScriptDataDoubleEscapedLessThanSign;
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                }
                Some(c) => self.emit(Token::Char(c)),
                None => {
                    self.error("eof in script html comment like text");
                    self.emit(Token::Eof);
                }
            },
            State::ScriptDataDoubleEscapedDash => match self.consume() {
                Some('-') => {
                    self.emit(Token::Char('-'));
                    self.state = State::ScriptDataDoubleEscapedDashDash;
                }
                Some('<') => {
                    self.emit(Token::Char('<'));
                    self.state = State::ScriptDataDoubleEscapedLessThanSign;
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                    self.state = State::ScriptDataDoubleEscaped;
                }
                Some(c) => {
                    self.emit(Token::Char(c));
                    self.state = State::ScriptDataDoubleEscaped;
                }
                None => {
                    self.error("eof in script html comment like text");
                    self.emit(Token::Eof);
                }
            },
            State::ScriptDataDoubleEscapedDashDash => match self.consume() {
                Some('-') => self.emit(Token::Char('-')),
                Some('<') => {
                    self.emit(Token::Char('<'));
                    self.state = State::ScriptDataDoubleEscapedLessThanSign;
                }
                Some('>') => {
                    self.emit(Token::Char('>'));
                    self.state = State::ScriptData;
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.emit(Token::Char(REPLACEMENT));
                    self.state = State::ScriptDataDoubleEscaped;
                }
                Some(c) => {
                    self.emit(Token::Char(c));
                    self.state = State::ScriptDataDoubleEscaped;
                }
                None => {
                    self.error("eof in script html comment like text");
                    self.emit(Token::Eof);
                }
            },
            State::ScriptDataDoubleEscapedLessThanSign => {
                if self.peek() == Some('/') {
                    self.pos += 1;
                    self.temp.clear();
                    self.emit(Token::Char('/'));
                    self.state = State::ScriptDataDoubleEscapeEnd;
                } else {
                    self.state = State::ScriptDataDoubleEscaped;
                }
            }
            State::ScriptDataDoubleEscapeEnd => {
                let c = self.consume();
                match c {
                    Some('\t') | Some('\n') | Some('\x0C') | Some(' ') | Some('/') | Some('>') => {
                        let temp = self.temp.clone();
                        self.state = if temp == "script" { State::ScriptDataEscaped } else { State::ScriptDataDoubleEscaped };
                        self.emit(Token::Char(c.unwrap()));
                    }
                    Some(c) if c.is_ascii_alphabetic() => {
                        self.temp.push(c.to_ascii_lowercase());
                        self.emit(Token::Char(c));
                    }
                    _ => self.reconsume(State::ScriptDataDoubleEscaped),
                }
            }
            State::BeforeAttributeName => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => {}
                Some('/') | Some('>') | None => self.reconsume(State::AfterAttributeName),
                Some('=') => {
                    self.error("unexpected equals sign before attribute name");
                    self.finish_attr();
                    self.attr = Some(Attr { name: "=".to_string(), value: String::new() });
                    self.state = State::AttributeName;
                }
                Some(_) => {
                    self.finish_attr();
                    self.attr = Some(Attr { name: String::new(), value: String::new() });
                    self.reconsume(State::AttributeName);
                }
            },
            State::AttributeName => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') | Some('/') | Some('>') | None => {
                    self.reconsume(State::AfterAttributeName);
                }
                Some('=') => self.state = State::BeforeAttributeValue,
                Some('\0') => {
                    self.error("unexpected null character");
                    self.push_attr_name(REPLACEMENT);
                }
                Some(c @ ('"' | '\'' | '<')) => {
                    self.error("unexpected character in attribute name");
                    self.push_attr_name(c);
                }
                Some(c) => self.push_attr_name(c.to_ascii_lowercase()),
            },
            State::AfterAttributeName => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => {}
                Some('/') => self.state = State::SelfClosingStartTag,
                Some('=') => self.state = State::BeforeAttributeValue,
                Some('>') => {
                    self.state = State::Data;
                    self.emit_tag();
                }
                Some(_) => {
                    self.finish_attr();
                    self.attr = Some(Attr { name: String::new(), value: String::new() });
                    self.reconsume(State::AttributeName);
                }
                None => {
                    self.error("eof in tag");
                    self.emit(Token::Eof);
                }
            },
            State::BeforeAttributeValue => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => {}
                Some('"') => self.state = State::AttributeValueDoubleQuoted,
                Some('\'') => self.state = State::AttributeValueSingleQuoted,
                Some('>') => {
                    self.error("missing attribute value");
                    self.state = State::Data;
                    self.emit_tag();
                }
                _ => self.reconsume(State::AttributeValueUnquoted),
            },
            State::AttributeValueDoubleQuoted => match self.consume() {
                Some('"') => self.state = State::AfterAttributeValueQuoted,
                Some('&') => {
                    self.return_state = State::AttributeValueDoubleQuoted;
                    self.state = State::CharacterReference;
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.push_attr_value(REPLACEMENT);
                }
                Some(c) => self.push_attr_value(c),
                None => {
                    self.error("eof in tag");
                    self.emit(Token::Eof);
                }
            },
            State::AttributeValueSingleQuoted => match self.consume() {
                Some('\'') => self.state = State::AfterAttributeValueQuoted,
                Some('&') => {
                    self.return_state = State::AttributeValueSingleQuoted;
                    self.state = State::CharacterReference;
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.push_attr_value(REPLACEMENT);
                }
                Some(c) => self.push_attr_value(c),
                None => {
                    self.error("eof in tag");
                    self.emit(Token::Eof);
                }
            },
            // `<a href=b?c=d>`: an unquoted value runs to whitespace or
            // `>`, so the `=` and `?` inside it are ordinary characters.
            State::AttributeValueUnquoted => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => self.state = State::BeforeAttributeName,
                Some('&') => {
                    self.return_state = State::AttributeValueUnquoted;
                    self.state = State::CharacterReference;
                }
                Some('>') => {
                    self.state = State::Data;
                    self.emit_tag();
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.push_attr_value(REPLACEMENT);
                }
                Some(c @ ('"' | '\'' | '<' | '=' | '`')) => {
                    self.error("unexpected character in unquoted attribute value");
                    self.push_attr_value(c);
                }
                Some(c) => self.push_attr_value(c),
                None => {
                    self.error("eof in tag");
                    self.emit(Token::Eof);
                }
            },
            State::AfterAttributeValueQuoted => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => self.state = State::BeforeAttributeName,
                Some('/') => self.state = State::SelfClosingStartTag,
                Some('>') => {
                    self.state = State::Data;
                    self.emit_tag();
                }
                Some(_) => {
                    self.error("missing whitespace between attributes");
                    self.reconsume(State::BeforeAttributeName);
                }
                None => {
                    self.error("eof in tag");
                    self.emit(Token::Eof);
                }
            },
            State::SelfClosingStartTag => match self.consume() {
                Some('>') => {
                    self.tag.self_closing = true;
                    self.state = State::Data;
                    self.emit_tag();
                }
                Some(_) => {
                    self.error("unexpected solidus in tag");
                    self.reconsume(State::BeforeAttributeName);
                }
                None => {
                    self.error("eof in tag");
                    self.emit(Token::Eof);
                }
            },
            // `<?php ... ?>` and `</3>` both land here: everything up to
            // the next `>` becomes a comment.
            State::BogusComment => match self.consume() {
                Some('>') => {
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.state = State::Data;
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.comment.push(REPLACEMENT);
                }
                Some(c) => self.comment.push(c),
                None => {
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.emit(Token::Eof);
                }
            },
            State::MarkupDeclarationOpen => {
                if self.starts_with("--") {
                    self.pos += 2;
                    self.comment.clear();
                    self.state = State::CommentStart;
                } else if self.starts_with_ascii_ci("DOCTYPE") {
                    self.pos += 7;
                    self.state = State::Doctype;
                } else if self.starts_with("[CDATA[") {
                    // Only meaningful in foreign content; in an HTML
                    // context the spec makes it a bogus comment, and the
                    // tree builder is what knows the difference (see
                    // Tokenizer::cdata_ok).
                    self.pos += 7;
                    if self.cdata_ok {
                        self.state = State::CdataSection;
                    } else {
                        self.error("cdata in html content");
                        self.comment = "[CDATA[".to_string();
                        self.state = State::BogusComment;
                    }
                } else {
                    self.error("incorrectly opened comment");
                    self.comment.clear();
                    self.state = State::BogusComment;
                }
            }
            State::CommentStart => match self.consume() {
                Some('-') => self.state = State::CommentStartDash,
                // `<!-->` is an empty comment, not text.
                Some('>') => {
                    self.error("abrupt closing of empty comment");
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.state = State::Data;
                }
                _ => self.reconsume(State::Comment),
            },
            State::CommentStartDash => match self.consume() {
                Some('-') => self.state = State::CommentEnd,
                Some('>') => {
                    self.error("abrupt closing of empty comment");
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.state = State::Data;
                }
                Some(_) => {
                    self.comment.push('-');
                    self.reconsume(State::Comment);
                }
                None => {
                    self.error("eof in comment");
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.emit(Token::Eof);
                }
            },
            State::Comment => match self.consume() {
                Some('<') => {
                    self.comment.push('<');
                    self.state = State::CommentLessThanSign;
                }
                Some('-') => self.state = State::CommentEndDash,
                Some('\0') => {
                    self.error("unexpected null character");
                    self.comment.push(REPLACEMENT);
                }
                Some(c) => self.comment.push(c),
                None => {
                    self.error("eof in comment");
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.emit(Token::Eof);
                }
            },
            State::CommentLessThanSign => match self.consume() {
                Some('!') => {
                    self.comment.push('!');
                    self.state = State::CommentLessThanSignBang;
                }
                Some('<') => self.comment.push('<'),
                _ => self.reconsume(State::Comment),
            },
            State::CommentLessThanSignBang => {
                if self.peek() == Some('-') {
                    self.pos += 1;
                    self.state = State::CommentLessThanSignBangDash;
                } else {
                    self.state = State::Comment;
                }
            }
            State::CommentLessThanSignBangDash => {
                if self.peek() == Some('-') {
                    self.pos += 1;
                    self.state = State::CommentLessThanSignBangDashDash;
                } else {
                    self.state = State::CommentEndDash;
                }
            }
            State::CommentLessThanSignBangDashDash => {
                if self.peek() != Some('>') && self.peek().is_some() {
                    self.error("nested comment");
                }
                self.state = State::CommentEnd;
            }
            State::CommentEndDash => match self.consume() {
                Some('-') => self.state = State::CommentEnd,
                Some(_) => {
                    self.comment.push('-');
                    self.reconsume(State::Comment);
                }
                None => {
                    self.error("eof in comment");
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.emit(Token::Eof);
                }
            },
            State::CommentEnd => match self.consume() {
                Some('>') => {
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.state = State::Data;
                }
                Some('!') => self.state = State::CommentEndBang,
                Some('-') => self.comment.push('-'),
                Some(_) => {
                    self.comment.push_str("--");
                    self.reconsume(State::Comment);
                }
                None => {
                    self.error("eof in comment");
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.emit(Token::Eof);
                }
            },
            State::CommentEndBang => match self.consume() {
                Some('-') => {
                    self.comment.push_str("--!");
                    self.state = State::CommentEndDash;
                }
                Some('>') => {
                    self.error("incorrectly closed comment");
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.state = State::Data;
                }
                Some(_) => {
                    self.comment.push_str("--!");
                    self.reconsume(State::Comment);
                }
                None => {
                    self.error("eof in comment");
                    let comment = std::mem::take(&mut self.comment);
                    self.emit(Token::Comment(comment));
                    self.emit(Token::Eof);
                }
            },
            State::Doctype => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => self.state = State::BeforeDoctypeName,
                Some('>') => self.reconsume(State::BeforeDoctypeName),
                Some(_) => {
                    self.error("missing whitespace before doctype name");
                    self.reconsume(State::BeforeDoctypeName);
                }
                None => {
                    self.error("eof in doctype");
                    self.new_doctype(true);
                    self.emit_doctype();
                    self.emit(Token::Eof);
                }
            },
            State::BeforeDoctypeName => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => {}
                Some('\0') => {
                    self.error("unexpected null character");
                    self.new_doctype(false);
                    self.doctype.name = Some(REPLACEMENT.to_string());
                    self.state = State::DoctypeName;
                }
                Some('>') => {
                    self.error("missing doctype name");
                    self.new_doctype(true);
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some(c) => {
                    self.new_doctype(false);
                    self.doctype.name = Some(c.to_ascii_lowercase().to_string());
                    self.state = State::DoctypeName;
                }
                None => {
                    self.error("eof in doctype");
                    self.new_doctype(true);
                    self.emit_doctype();
                    self.emit(Token::Eof);
                }
            },
            State::DoctypeName => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => self.state = State::AfterDoctypeName,
                Some('>') => {
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some('\0') => {
                    self.error("unexpected null character");
                    self.push_doctype_name(REPLACEMENT);
                }
                Some(c) => self.push_doctype_name(c.to_ascii_lowercase()),
                None => {
                    self.error("eof in doctype");
                    self.doctype.force_quirks = true;
                    self.emit_doctype();
                    self.emit(Token::Eof);
                }
            },
            State::AfterDoctypeName => {
                if matches!(self.peek(), Some('\t') | Some('\n') | Some('\x0C') | Some(' ')) {
                    self.pos += 1;
                } else if self.peek() == Some('>') {
                    self.pos += 1;
                    self.emit_doctype();
                    self.state = State::Data;
                } else if self.peek().is_none() {
                    self.error("eof in doctype");
                    self.doctype.force_quirks = true;
                    self.emit_doctype();
                    self.emit(Token::Eof);
                } else if self.starts_with_ascii_ci("PUBLIC") {
                    self.pos += 6;
                    self.state = State::AfterDoctypePublicKeyword;
                } else if self.starts_with_ascii_ci("SYSTEM") {
                    self.pos += 6;
                    self.state = State::AfterDoctypeSystemKeyword;
                } else {
                    self.error("invalid character sequence after doctype name");
                    self.doctype.force_quirks = true;
                    self.state = State::BogusDoctype;
                }
            }
            State::AfterDoctypePublicKeyword => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => self.state = State::BeforeDoctypePublicIdentifier,
                Some('"') => {
                    self.error("missing whitespace after doctype public keyword");
                    self.doctype.public_id = Some(String::new());
                    self.state = State::DoctypePublicIdentifierDoubleQuoted;
                }
                Some('\'') => {
                    self.error("missing whitespace after doctype public keyword");
                    self.doctype.public_id = Some(String::new());
                    self.state = State::DoctypePublicIdentifierSingleQuoted;
                }
                Some('>') => {
                    self.error("missing doctype public identifier");
                    self.doctype.force_quirks = true;
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some(_) => {
                    self.error("missing quote before doctype public identifier");
                    self.doctype.force_quirks = true;
                    self.reconsume(State::BogusDoctype);
                }
                None => self.doctype_eof(),
            },
            State::BeforeDoctypePublicIdentifier => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => {}
                Some('"') => {
                    self.doctype.public_id = Some(String::new());
                    self.state = State::DoctypePublicIdentifierDoubleQuoted;
                }
                Some('\'') => {
                    self.doctype.public_id = Some(String::new());
                    self.state = State::DoctypePublicIdentifierSingleQuoted;
                }
                Some('>') => {
                    self.error("missing doctype public identifier");
                    self.doctype.force_quirks = true;
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some(_) => {
                    self.error("missing quote before doctype public identifier");
                    self.doctype.force_quirks = true;
                    self.reconsume(State::BogusDoctype);
                }
                None => self.doctype_eof(),
            },
            State::DoctypePublicIdentifierDoubleQuoted => self.doctype_id(true, '"'),
            State::DoctypePublicIdentifierSingleQuoted => self.doctype_id(true, '\''),
            State::AfterDoctypePublicIdentifier => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => self.state = State::BetweenDoctypePublicAndSystemIdentifiers,
                Some('>') => {
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some('"') => {
                    self.error("missing whitespace between doctype public and system identifiers");
                    self.doctype.system_id = Some(String::new());
                    self.state = State::DoctypeSystemIdentifierDoubleQuoted;
                }
                Some('\'') => {
                    self.error("missing whitespace between doctype public and system identifiers");
                    self.doctype.system_id = Some(String::new());
                    self.state = State::DoctypeSystemIdentifierSingleQuoted;
                }
                Some(_) => {
                    self.error("missing quote before doctype system identifier");
                    self.doctype.force_quirks = true;
                    self.reconsume(State::BogusDoctype);
                }
                None => self.doctype_eof(),
            },
            State::BetweenDoctypePublicAndSystemIdentifiers => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => {}
                Some('>') => {
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some('"') => {
                    self.doctype.system_id = Some(String::new());
                    self.state = State::DoctypeSystemIdentifierDoubleQuoted;
                }
                Some('\'') => {
                    self.doctype.system_id = Some(String::new());
                    self.state = State::DoctypeSystemIdentifierSingleQuoted;
                }
                Some(_) => {
                    self.error("missing quote before doctype system identifier");
                    self.doctype.force_quirks = true;
                    self.reconsume(State::BogusDoctype);
                }
                None => self.doctype_eof(),
            },
            State::AfterDoctypeSystemKeyword => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => self.state = State::BeforeDoctypeSystemIdentifier,
                Some('"') => {
                    self.error("missing whitespace after doctype system keyword");
                    self.doctype.system_id = Some(String::new());
                    self.state = State::DoctypeSystemIdentifierDoubleQuoted;
                }
                Some('\'') => {
                    self.error("missing whitespace after doctype system keyword");
                    self.doctype.system_id = Some(String::new());
                    self.state = State::DoctypeSystemIdentifierSingleQuoted;
                }
                Some('>') => {
                    self.error("missing doctype system identifier");
                    self.doctype.force_quirks = true;
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some(_) => {
                    self.error("missing quote before doctype system identifier");
                    self.doctype.force_quirks = true;
                    self.reconsume(State::BogusDoctype);
                }
                None => self.doctype_eof(),
            },
            State::BeforeDoctypeSystemIdentifier => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => {}
                Some('"') => {
                    self.doctype.system_id = Some(String::new());
                    self.state = State::DoctypeSystemIdentifierDoubleQuoted;
                }
                Some('\'') => {
                    self.doctype.system_id = Some(String::new());
                    self.state = State::DoctypeSystemIdentifierSingleQuoted;
                }
                Some('>') => {
                    self.error("missing doctype system identifier");
                    self.doctype.force_quirks = true;
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some(_) => {
                    self.error("missing quote before doctype system identifier");
                    self.doctype.force_quirks = true;
                    self.reconsume(State::BogusDoctype);
                }
                None => self.doctype_eof(),
            },
            State::DoctypeSystemIdentifierDoubleQuoted => self.doctype_id(false, '"'),
            State::DoctypeSystemIdentifierSingleQuoted => self.doctype_id(false, '\''),
            State::AfterDoctypeSystemIdentifier => match self.consume() {
                Some('\t') | Some('\n') | Some('\x0C') | Some(' ') => {}
                Some('>') => {
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some(_) => {
                    self.error("unexpected character after doctype system identifier");
                    self.reconsume(State::BogusDoctype);
                }
                None => self.doctype_eof(),
            },
            State::BogusDoctype => match self.consume() {
                Some('>') => {
                    self.emit_doctype();
                    self.state = State::Data;
                }
                Some('\0') => self.error("unexpected null character"),
                Some(_) => {}
                None => {
                    self.emit_doctype();
                    self.emit(Token::Eof);
                }
            },
            State::CdataSection => match self.consume() {
                Some(']') => self.state = State::CdataSectionBracket,
                Some(c) => self.emit(Token::Char(c)),
                None => {
                    self.error("eof in cdata");
                    self.emit(Token::Eof);
                }
            },
            State::CdataSectionBracket => {
                if self.peek() == Some(']') {
                    self.pos += 1;
                    self.state = State::CdataSectionEnd;
                } else {
                    self.emit(Token::Char(']'));
                    self.state = State::CdataSection;
                }
            }
            State::CdataSectionEnd => match self.peek() {
                Some(']') => {
                    self.pos += 1;
                    self.emit(Token::Char(']'));
                }
                Some('>') => {
                    self.pos += 1;
                    self.state = State::Data;
                }
                _ => {
                    self.emit_chars("]]");
                    self.state = State::CdataSection;
                }
            },
            State::CharacterReference => {
                self.temp = "&".to_string();
                match self.peek() {
                    Some(c) if c.is_ascii_alphanumeric() => self.state = State::NamedCharacterReference,
                    Some('#') => {
                        self.pos += 1;
                        self.temp.push('#');
                        self.state = State::NumericCharacterReference;
                    }
                    _ => {
                        self.flush_char_ref();
                        self.state = self.return_state;
                    }
                }
            }
            // The spec's own longest-match rule, which is why the whole
            // 2231-entry table has to be present: `&notin;` is one
            // reference, `&notit;` is `&not` plus "it;".
            State::NamedCharacterReference => {
                let rest: String = self.input[self.pos..].iter().take(entities::MAX_NAME_LEN).collect();
                match longest_named_match(&rest) {
                    Some((name, value)) => {
                        self.pos += name.chars().count();
                        self.temp.push_str(name);
                        let terminated = name.ends_with(';');
                        // "If the character reference was consumed as
                        // part of an attribute, and the last character
                        // matched is not a semicolon, and the next input
                        // character is `=` or alphanumeric, flush as-is"
                        // -- what keeps `?a&not=b` a literal query
                        // string rather than turning it into `?a\u{AC}=b`.
                        let next = self.peek();
                        if !terminated && self.in_attribute() && next.is_some_and(|c| c == '=' || c.is_ascii_alphanumeric()) {
                            self.flush_char_ref();
                            self.state = self.return_state;
                        } else {
                            if !terminated {
                                self.error("missing semicolon after character reference");
                            }
                            self.temp = value.to_string();
                            self.flush_char_ref();
                            self.state = self.return_state;
                        }
                    }
                    None => {
                        self.flush_char_ref();
                        self.state = State::AmbiguousAmpersand;
                    }
                }
            }
            State::AmbiguousAmpersand => match self.peek() {
                Some(c) if c.is_ascii_alphanumeric() => {
                    self.pos += 1;
                    if self.in_attribute() {
                        if let Some(attr) = self.attr.as_mut() {
                            attr.value.push(c);
                        }
                    } else {
                        self.emit(Token::Char(c));
                    }
                }
                Some(';') => {
                    self.error("unknown named character reference");
                    self.state = self.return_state;
                }
                _ => self.state = self.return_state,
            },
            State::NumericCharacterReference => {
                self.char_ref_code = 0;
                match self.peek() {
                    Some(c @ ('x' | 'X')) => {
                        self.pos += 1;
                        self.temp.push(c);
                        self.state = State::HexadecimalCharacterReferenceStart;
                    }
                    _ => self.state = State::DecimalCharacterReferenceStart,
                }
            }
            State::HexadecimalCharacterReferenceStart => {
                if self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                    self.state = State::HexadecimalCharacterReference;
                } else {
                    self.error("absence of digits in numeric character reference");
                    self.flush_char_ref();
                    self.state = self.return_state;
                }
            }
            State::DecimalCharacterReferenceStart => {
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.state = State::DecimalCharacterReference;
                } else {
                    self.error("absence of digits in numeric character reference");
                    self.flush_char_ref();
                    self.state = self.return_state;
                }
            }
            State::HexadecimalCharacterReference => match self.consume() {
                Some(c) if c.is_ascii_hexdigit() => {
                    self.char_ref_code = self.char_ref_code.saturating_mul(16).saturating_add(c.to_digit(16).unwrap());
                }
                Some(';') => self.state = State::NumericCharacterReferenceEnd,
                _ => self.reconsume(State::NumericCharacterReferenceEnd),
            },
            State::DecimalCharacterReference => match self.consume() {
                Some(c) if c.is_ascii_digit() => {
                    self.char_ref_code = self.char_ref_code.saturating_mul(10).saturating_add(c.to_digit(10).unwrap());
                }
                Some(';') => self.state = State::NumericCharacterReferenceEnd,
                _ => self.reconsume(State::NumericCharacterReferenceEnd),
            },
            State::NumericCharacterReferenceEnd => {
                // Reached either by consuming the `;` or by reconsuming
                // whatever came instead of one, which is a parse error
                // the spec attributes to this state.
                if self.input.get(self.pos.wrapping_sub(1)) != Some(&';') {
                    self.error("missing semicolon after character reference");
                }
                let code = self.char_ref_code;
                let c = numeric_reference_char(code, &mut |e| self.errors.push(e.to_string()));
                self.temp = c.to_string();
                self.flush_char_ref();
                self.state = self.return_state;
            }
        }
    }

    // The tail shared by the six `*EndTagName` states: build the name,
    // and if it turns out not to match the element that opened, spit
    // everything back out as text.
    fn end_tag_name(&mut self, fallback: State) {
        match self.consume() {
            Some(c @ ('\t' | '\n' | '\x0C' | ' ')) if self.appropriate_end_tag() => {
                let _ = c;
                self.state = State::BeforeAttributeName;
            }
            Some('/') if self.appropriate_end_tag() => self.state = State::SelfClosingStartTag,
            Some('>') if self.appropriate_end_tag() => {
                self.state = State::Data;
                self.emit_tag();
            }
            Some(c) if c.is_ascii_alphabetic() => {
                self.tag.name.push(c.to_ascii_lowercase());
                self.temp.push(c);
            }
            _ => {
                self.emit_chars("</");
                let temp = std::mem::take(&mut self.temp);
                self.emit_chars(&temp);
                self.reconsume(fallback);
            }
        }
    }

    fn doctype_id(&mut self, public: bool, quote: char) {
        let c = self.consume();
        let slot = if public { &mut self.doctype.public_id } else { &mut self.doctype.system_id };
        match c {
            Some(c) if c == quote => self.state = if public { State::AfterDoctypePublicIdentifier } else { State::AfterDoctypeSystemIdentifier },
            Some('\0') => {
                slot.get_or_insert_with(String::new).push(REPLACEMENT);
                self.error("unexpected null character");
            }
            Some('>') => {
                self.doctype.force_quirks = true;
                self.error("abrupt doctype identifier");
                self.emit_doctype();
                self.state = State::Data;
            }
            Some(c) => slot.get_or_insert_with(String::new).push(c),
            None => self.doctype_eof(),
        }
    }

    fn doctype_eof(&mut self) {
        self.error("eof in doctype");
        self.doctype.force_quirks = true;
        self.emit_doctype();
        self.emit(Token::Eof);
    }

    fn new_doctype(&mut self, force_quirks: bool) {
        self.doctype = Doctype { name: None, public_id: None, system_id: None, force_quirks };
    }

    fn emit_doctype(&mut self) {
        let d = std::mem::replace(&mut self.doctype, Doctype { name: None, public_id: None, system_id: None, force_quirks: false });
        self.emit(Token::Doctype(d));
    }

    fn push_doctype_name(&mut self, c: char) {
        self.doctype.name.get_or_insert_with(String::new).push(c);
    }

    fn push_attr_name(&mut self, c: char) {
        if let Some(attr) = self.attr.as_mut() {
            attr.name.push(c);
        }
    }

    fn push_attr_value(&mut self, c: char) {
        if let Some(attr) = self.attr.as_mut() {
            attr.value.push(c);
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        self.input[self.pos.min(self.input.len())..].iter().take(s.chars().count()).eq(s.chars().collect::<Vec<_>>().iter())
    }

    fn starts_with_ascii_ci(&self, s: &str) -> bool {
        let here: String = self.input[self.pos.min(self.input.len())..].iter().take(s.chars().count()).collect();
        here.eq_ignore_ascii_case(s)
    }
}

// §13.2.3.5: normalize CRLF and lone CR to LF before tokenizing.
fn normalize_newlines(input: &str) -> Vec<char> {
    let mut out = Vec::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

// The longest entry in the table that prefixes `rest`. Binary search
// finds *a* neighbourhood; the table is sorted, so the longest match is
// found by walking candidate lengths down from the longest possible.
fn longest_named_match(rest: &str) -> Option<(&'static str, &'static str)> {
    let chars: Vec<char> = rest.chars().collect();
    for len in (1..=chars.len().min(entities::MAX_NAME_LEN)).rev() {
        let candidate: String = chars[..len].iter().collect();
        if let Ok(i) = entities::NAMED.binary_search_by(|(name, _)| name.cmp(&candidate.as_str())) {
            return Some(entities::NAMED[i]);
        }
    }
    None
}

// §13.2.5.80's table of numeric references that don't mean what they
// say: NULL and out-of-range become U+FFFD, surrogates are invalid, and
// the C1 range 0x80-0x9F is remapped to the windows-1252 characters
// authors actually meant (a real-world compatibility rule, not a
// mistake).
fn numeric_reference_char(code: u32, error: &mut dyn FnMut(&str)) -> char {
    const C1: [char; 32] = [
        '\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{2C6}', '\u{2030}', '\u{160}', '\u{2039}',
        '\u{152}', '\u{8D}', '\u{17D}', '\u{8F}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
        '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}', '\u{17E}', '\u{178}',
    ];
    match code {
        0 => {
            error("null character reference");
            REPLACEMENT
        }
        c if c > 0x10FFFF => {
            error("character reference outside unicode range");
            REPLACEMENT
        }
        0xD800..=0xDFFF => {
            error("surrogate character reference");
            REPLACEMENT
        }
        0x80..=0x9F => {
            error("control character reference");
            C1[(code - 0x80) as usize]
        }
        c => {
            let ch = char::from_u32(c).unwrap_or(REPLACEMENT);
            if is_noncharacter(c) {
                error("noncharacter character reference");
            } else if c != 0x0D && is_control(c) && !ch.is_whitespace() {
                error("control character reference");
            }
            ch
        }
    }
}

fn is_noncharacter(c: u32) -> bool {
    (0xFDD0..=0xFDEF).contains(&c) || (c & 0xFFFE) == 0xFFFE
}

fn is_control(c: u32) -> bool {
    c < 0x20 || (0x7F..=0x9F).contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every token, with character runs coalesced into strings so a test
    // reads as the markup it describes rather than as a list of chars.
    #[derive(Debug, PartialEq, Eq)]
    enum T {
        Text(String),
        Start(String, Vec<(String, String)>, bool),
        End(String),
        Comment(String),
        Doctype(Option<String>, Option<String>, Option<String>, bool),
    }

    fn tokenize(input: &str) -> Vec<T> {
        tokenize_with(input, ContentState::Data)
    }

    fn tokenize_with(input: &str, initial: ContentState) -> Vec<T> {
        let mut tk = Tokenizer::new(input);
        tk.set_state(initial);
        let mut out: Vec<T> = Vec::new();
        loop {
            match tk.next() {
                Token::Eof => return out,
                Token::Char(c) => match out.last_mut() {
                    Some(T::Text(s)) => s.push(c),
                    _ => out.push(T::Text(c.to_string())),
                },
                Token::StartTag(tag) => {
                    // A start tag switching the tokenizer's content
                    // model is normally the tree builder's job; these
                    // tests do it here so RCDATA/RAWTEXT/script data can
                    // be exercised at all.
                    match tag.name.as_str() {
                        "title" | "textarea" => tk.set_state(ContentState::Rcdata),
                        "style" | "xmp" | "iframe" | "noembed" | "noframes" => tk.set_state(ContentState::Rawtext),
                        "script" => tk.set_state(ContentState::ScriptData),
                        "plaintext" => tk.set_state(ContentState::Plaintext),
                        _ => {}
                    }
                    let attrs = tag.attrs.iter().map(|a| (a.name.clone(), a.value.clone())).collect();
                    out.push(T::Start(tag.name, attrs, tag.self_closing));
                }
                Token::EndTag(tag) => out.push(T::End(tag.name)),
                Token::Comment(c) => out.push(T::Comment(c)),
                Token::Doctype(d) => out.push(T::Doctype(d.name, d.public_id, d.system_id, d.force_quirks)),
            }
        }
    }

    fn text(s: &str) -> T {
        T::Text(s.to_string())
    }

    fn start(name: &str, attrs: &[(&str, &str)]) -> T {
        T::Start(name.to_string(), attrs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(), false)
    }

    #[test]
    fn tokenizes_a_plain_element_with_attributes() {
        assert_eq!(
            tokenize(r#"<a href="x" class='y'>hi</a>"#),
            vec![start("a", &[("href", "x"), ("class", "y")]), text("hi"), T::End("a".to_string())]
        );
    }

    #[test]
    fn tag_and_attribute_names_are_lowercased_but_values_are_not() {
        assert_eq!(tokenize(r#"<DIV CLASS="Big">"#), vec![start("div", &[("class", "Big")])]);
    }

    // An unquoted value runs to whitespace or `>`, so `=` and `?` inside
    // it are ordinary characters -- the case a naive `split('=')` gets
    // wrong.
    #[test]
    fn an_unquoted_attribute_value_may_contain_equals_and_question_marks() {
        assert_eq!(tokenize("<a href=b?c=d&e=f>"), vec![start("a", &[("href", "b?c=d&e=f")])]);
    }

    #[test]
    fn a_self_closing_tag_is_marked_as_such() {
        assert_eq!(tokenize("<br/>"), vec![T::Start("br".to_string(), vec![], true)]);
        assert_eq!(tokenize("<br>"), vec![T::Start("br".to_string(), vec![], false)]);
    }

    #[test]
    fn an_empty_attribute_has_an_empty_value() {
        assert_eq!(tokenize("<input disabled>"), vec![start("input", &[("disabled", "")])]);
        assert_eq!(tokenize("<input disabled=>"), vec![start("input", &[("disabled", "")])]);
    }

    #[test]
    fn a_duplicate_attribute_keeps_the_first() {
        let mut tk = Tokenizer::new(r#"<a x="1" x="2">"#);
        assert_eq!(
            tk.next(),
            Token::StartTag(Tag { name: "a".to_string(), attrs: vec![Attr { name: "x".to_string(), value: "1".to_string() }], self_closing: false })
        );
        assert!(tk.errors.iter().any(|e| e.contains("duplicate")));
    }

    // A `<` that starts nothing is text, which is what keeps prose like
    // `a < b` intact.
    #[test]
    fn a_less_than_that_starts_no_tag_is_literal_text() {
        assert_eq!(tokenize("a < b"), vec![text("a < b")]);
        assert_eq!(tokenize("5<6"), vec![text("5<6")]);
    }

    #[test]
    fn comments_including_the_empty_and_abrupt_forms() {
        assert_eq!(tokenize("<!-- hi -->"), vec![T::Comment(" hi ".to_string())]);
        assert_eq!(tokenize("<!-->"), vec![T::Comment(String::new())]);
        assert_eq!(tokenize("<!--->"), vec![T::Comment(String::new())]);
        assert_eq!(tokenize("<!--a--!>"), vec![T::Comment("a".to_string())]);
        // A dash inside doesn't end it.
        assert_eq!(tokenize("<!--a-b-->"), vec![T::Comment("a-b".to_string())]);
    }

    // `<?php ... ?>` isn't a processing instruction in HTML -- it's a
    // bogus comment, which is exactly why a preview can drop it whole.
    #[test]
    fn a_processing_instruction_becomes_a_bogus_comment() {
        assert_eq!(tokenize("<?php echo 1; ?>"), vec![T::Comment("?php echo 1; ?".to_string())]);
        assert_eq!(tokenize("</3>"), vec![T::Comment("3".to_string())]);
    }

    #[test]
    fn doctypes_with_and_without_identifiers() {
        assert_eq!(tokenize("<!DOCTYPE html>"), vec![T::Doctype(Some("html".to_string()), None, None, false)]);
        assert_eq!(
            tokenize(r#"<!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.01//EN" "http://www.w3.org/TR/html4/strict.dtd">"#),
            vec![T::Doctype(
                Some("html".to_string()),
                Some("-//W3C//DTD HTML 4.01//EN".to_string()),
                Some("http://www.w3.org/TR/html4/strict.dtd".to_string()),
                false
            )]
        );
        // Missing name: force-quirks.
        assert_eq!(tokenize("<!DOCTYPE>"), vec![T::Doctype(None, None, None, true)]);
    }

    // The reason the whole 2231-entry table has to be there: `&notin;`
    // is one reference, but `&notit;` matches the *legacy* `&not` and
    // leaves "it;" as text.
    #[test]
    fn named_character_references_take_the_longest_match() {
        assert_eq!(tokenize("&notin;"), vec![text("\u{2209}")]);
        assert_eq!(tokenize("&notit;"), vec![text("\u{AC}it;")]);
        assert_eq!(tokenize("&amp;"), vec![text("&")]);
        assert_eq!(tokenize("&AMP"), vec![text("&")]);
        // Two code points from one reference.
        assert_eq!(tokenize("&NotEqualTilde;"), vec![text("\u{2242}\u{338}")]);
        // Not a reference at all: left completely alone.
        assert_eq!(tokenize("&nope;"), vec![text("&nope;")]);
        assert_eq!(tokenize("a & b"), vec![text("a & b")]);
    }

    // In an attribute, an unterminated reference followed by `=` or a
    // letter stays literal -- what keeps a query string a query string.
    #[test]
    fn an_unterminated_reference_in_an_attribute_stays_literal() {
        assert_eq!(tokenize("<a href='?a&not=b'>"), vec![start("a", &[("href", "?a&not=b")])]);
        // ...but with the semicolon it really is a reference.
        assert_eq!(tokenize("<a href='?a&not;=b'>"), vec![start("a", &[("href", "?a\u{AC}=b")])]);
    }

    #[test]
    fn numeric_character_references_including_the_spec_remappings() {
        assert_eq!(tokenize("&#65;"), vec![text("A")]);
        assert_eq!(tokenize("&#x41;"), vec![text("A")]);
        assert_eq!(tokenize("&#X41;"), vec![text("A")]);
        // NULL and out-of-range become U+FFFD.
        assert_eq!(tokenize("&#0;"), vec![text("\u{FFFD}")]);
        assert_eq!(tokenize("&#x110000;"), vec![text("\u{FFFD}")]);
        // A surrogate is not a character.
        assert_eq!(tokenize("&#xD800;"), vec![text("\u{FFFD}")]);
        // The windows-1252 remapping of the C1 range: &#128; is the euro
        // sign, not U+0080.
        assert_eq!(tokenize("&#128;"), vec![text("\u{20AC}")]);
        // No digits at all: literal.
        assert_eq!(tokenize("&#x;"), vec![text("&#x;")]);
    }

    // RCDATA: references resolve, tags don't -- except the one that
    // closes the element.
    #[test]
    fn rcdata_keeps_tags_as_text_but_still_expands_references() {
        assert_eq!(tokenize("<title>a <b> &amp; c</title>d"), vec![start("title", &[]), text("a <b> & c"), T::End("title".to_string()), text("d")]);
    }

    #[test]
    fn rawtext_keeps_everything_as_text() {
        assert_eq!(
            tokenize("<style>a <b> &amp; c</style>d"),
            vec![start("style", &[]), text("a <b> &amp; c"), T::End("style".to_string()), text("d")]
        );
    }

    // The case that makes script data its own content model: a `</div>`
    // inside a script is script text, and only `</script>` ends it.
    #[test]
    fn script_data_ends_only_at_its_own_end_tag() {
        assert_eq!(
            tokenize("<script>if (a</b) { x('</div>') }</script>after"),
            vec![start("script", &[]), text("if (a</b) { x('</div>') }"), T::End("script".to_string()), text("after")]
        );
    }

    // And the escaped states: inside `<!--`, a nested `<script>` opens a
    // double-escape where `</script>` no longer closes the outer one.
    #[test]
    fn script_data_handles_the_escaped_and_double_escaped_forms() {
        assert_eq!(
            tokenize("<script><!-- <script> </script> --></script>x"),
            vec![start("script", &[]), text("<!-- <script> </script> -->"), T::End("script".to_string()), text("x")]
        );
    }

    #[test]
    fn a_mismatched_end_tag_in_rawtext_comes_back_out_as_text() {
        assert_eq!(tokenize("<style>a</styl</style>"), vec![start("style", &[]), text("a</styl"), T::End("style".to_string())]);
    }

    #[test]
    fn crlf_and_lone_cr_are_normalized_to_lf() {
        assert_eq!(tokenize("a\r\nb\rc"), vec![text("a\nb\nc")]);
    }

    #[test]
    fn plaintext_swallows_the_rest_of_the_document() {
        assert_eq!(tokenize("<plaintext>a<b>c"), vec![start("plaintext", &[]), text("a<b>c")]);
    }

    // Truncated input has to terminate, not spin -- every state's EOF
    // arm eventually emits Eof.
    #[test]
    fn every_truncation_of_a_document_terminates() {
        let doc = r#"<!DOCTYPE html><html><body class="x"><!-- c --><script>a<b</script>&amp;&#65;<p/></body>"#;
        for cut in 0..=doc.len() {
            if !doc.is_char_boundary(cut) {
                continue;
            }
            // The assertion is simply that this returns at all.
            let _ = tokenize(&doc[..cut]);
        }
    }
}
