// Tab-completion (see plan.md). Foundation stage: the candidate/provider
// types every source will implement against, plus the cursor-targeted
// word-role walker (`find_word_start` / `classify_word_role`) that decides
// *what kind* of completion applies to the word under the cursor --
// command name, flag, subcommand, or file. The actual shell-backed
// candidate sources are a later stage in this same file.
#![allow(dead_code)]

use crate::bishedit::highlight::{is_assignment_prefix_word, resets_command_position};
use crate::lexer::{self, Chunk, SpannedItem, Tok};

#[derive(Debug, Clone, PartialEq)]
pub struct CompletionCandidate {
    pub display: String,
    pub matched_positions: Vec<usize>,
}

pub struct CompletionRequest<'a> {
    pub line: &'a str,
    // Char index (not byte offset) into `line`'s chars() sequence --
    // matches LineEditor's own cursor semantics, since that's this
    // request's real origin.
    pub cursor: usize,
}

pub struct CompletionResult {
    // Char index. `word_start..cursor` is always the replaced range -- v1
    // never touches text after the cursor within the same word.
    pub word_start: usize,
    pub candidates: Vec<CompletionCandidate>,
}

pub trait CompletionProvider {
    fn complete(&self, req: CompletionRequest) -> CompletionResult;
}

// Whitespace and `| & ; ( ) < >` are word boundaries; quotes/`$`/backslash
// are deliberately NOT -- a documented v1 gap (this scans plain characters,
// it doesn't understand quoting), matching the same "no crash, just a
// plausible-not-guaranteed answer" tolerance the rest of this feature
// leans on for anything quote/substitution-shaped.
fn is_word_char(c: char) -> bool {
    !(c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>'))
}

// Scans backward from `cursor` over word characters to the enclosing
// boundary. Always lands exactly on a boundary (or 0), which is what lets
// `classify_word_role` treat `chars[0..word_start]` as a clean,
// fully-terminated prefix safe to re-tokenize on its own.
pub(crate) fn find_word_start(chars: &[char], cursor: usize) -> usize {
    let mut i = cursor.min(chars.len());
    while i > 0 && is_word_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CmdRole {
    Command,
    Argument { command: Option<String>, arg_index: usize },
}

enum CmdPosLite {
    ExpectCommand,
    InCommand { name: Option<String>, arg_index: usize },
}

// Re-tokenizes `prefix_text` (always a clean prefix ending right at a word
// boundary, per find_word_start) and walks it with the same
// ExpectCommand/InCommand shape highlight.rs's own CmdPos state machine
// uses -- reusing its now-pub(crate) `resets_command_position` /
// `is_assignment_prefix_word` primitives rather than sharing that
// module's actual stateful walker, which is built for a different job
// (emitting spans across a whole line, not answering "what's the role of
// the word starting right after this text"). The *terminal* state after
// walking every token in `prefix_text` is the role of the word being
// completed.
//
// Tolerates anything tokenize_spanned tolerates: an unclosed `$(...)` etc.
// degrades to whatever partial token stream the lexer managed to collect
// before erroring (see SpannedResult::error's own doc comment) -- never a
// panic, just a role that may not be the "real" one a full parse would
// give.
pub(crate) fn classify_word_role(prefix_text: &str) -> CmdRole {
    let res = lexer::tokenize_spanned(prefix_text);
    let mut cmd_pos = CmdPosLite::ExpectCommand;
    for item in &res.items {
        let SpannedItem::Tok(tok, _) = item else { continue };
        if resets_command_position(tok) {
            cmd_pos = CmdPosLite::ExpectCommand;
        }
        if let Tok::Word(chunks, _) = tok {
            cmd_pos = match cmd_pos {
                CmdPosLite::ExpectCommand if is_assignment_prefix_word(chunks) => CmdPosLite::ExpectCommand,
                CmdPosLite::ExpectCommand => {
                    let name = if let [Chunk::Str(s)] = chunks.as_slice() { Some(s.clone()) } else { None };
                    CmdPosLite::InCommand { name, arg_index: 0 }
                }
                CmdPosLite::InCommand { name, arg_index } => CmdPosLite::InCommand { name, arg_index: arg_index + 1 },
            };
        }
    }
    match cmd_pos {
        CmdPosLite::ExpectCommand => CmdRole::Command,
        CmdPosLite::InCommand { name, arg_index } => CmdRole::Argument { command: name, arg_index },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_word_start_scans_back_to_whitespace() {
        let chars: Vec<char> = "git checkout".chars().collect();
        assert_eq!(find_word_start(&chars, chars.len()), 4); // start of "checkout"
        assert_eq!(find_word_start(&chars, 4), 4); // already at boundary
        assert_eq!(find_word_start(&chars, 0), 0);
    }

    #[test]
    fn find_word_start_treats_pipe_and_semicolon_as_boundaries() {
        let chars: Vec<char> = "true|false".chars().collect();
        assert_eq!(find_word_start(&chars, chars.len()), 5); // start of "false"
        let chars: Vec<char> = "true;false".chars().collect();
        assert_eq!(find_word_start(&chars, chars.len()), 5);
    }

    #[test]
    fn classify_word_role_bare_prefix_is_command_position() {
        assert_eq!(classify_word_role(""), CmdRole::Command);
    }

    #[test]
    fn classify_word_role_git_co_is_arg_zero_of_git() {
        // The plan's own worked example: "git co" with the cursor at the
        // end -- word_start is 4 ("co" starts there), and prefix_text
        // (chars[0..4]) is "git ".
        let line = "git co";
        let chars: Vec<char> = line.chars().collect();
        let cursor = chars.len();
        let word_start = find_word_start(&chars, cursor);
        assert_eq!(word_start, 4);
        let prefix_text: String = chars[..word_start].iter().collect();
        assert_eq!(prefix_text, "git ");
        assert_eq!(
            classify_word_role(&prefix_text),
            CmdRole::Argument { command: Some("git".to_string()), arg_index: 0 }
        );
    }

    #[test]
    fn classify_word_role_resets_to_command_after_a_pipe() {
        assert_eq!(classify_word_role("true | "), CmdRole::Command);
    }

    #[test]
    fn classify_word_role_skips_an_assignment_prefix() {
        // "FOO=bar " leaves the position still expecting the real command
        // name, not treating FOO=bar as arg 0 of anything.
        assert_eq!(classify_word_role("FOO=bar "), CmdRole::Command);
    }

    #[test]
    fn classify_word_role_degrades_gracefully_inside_an_unclosed_substitution() {
        // Must not panic; the exact role isn't asserted since an unclosed
        // $(...) is documented out-of-scope for a guaranteed-correct
        // answer here -- only that this stays inert.
        let _ = classify_word_role("echo $(git chec");
    }
}
