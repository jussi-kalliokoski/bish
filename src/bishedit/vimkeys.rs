// Translates a sequence of editor::Key events into bishedit::motion::Motion
// values. This layer knows vim's specific bindings and owns the state a
// single keypress can't (an accumulating count, a pending g/f/F/t/T/m/`/'
// prefix awaiting one more character, an in-progress search string) -- but
// it never touches a Buffer or a terminal. Motions themselves are applied
// by the caller via bishedit::motion::apply_motion.

use crate::editor::Key;
use super::motion::Motion;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// A motion is ready to apply, with the raw count the user typed before
    /// it (if any).
    Motion(Motion, Option<usize>),
    /// A Ctrl-W window command is ready to run, with a count. Not a
    /// `Motion`: these act on the frontend's own window/pane state, not
    /// on a `Buffer`, so they're a separate outcome the caller applies
    /// however it applies window commands (repl.rs already has
    /// `apply_window_action` for exactly this). For most `WindowCmd`
    /// variants the count is a *repeat*, typed before `<C-w>` -- e.g.
    /// `2<C-w>n` is `WindowCmd::Next` with count `Some(2)`, "next window,
    /// twice". `GotoFirstWindow`/`GotoLastWindow` are the exception,
    /// mirroring `Motion::GotoFirstLine`/`GotoLastLine`: their count is
    /// typed *inside* the `<C-w>` sequence, after it, as an absolute
    /// 1-indexed tab position -- `<C-w>5gg` and `<C-w>5G` both mean "go
    /// to the 5th tab" (count `Some(5)`), while bare `<C-w>gg`/`<C-w>G`
    /// (count `None`) default to the first/last tab respectively.
    Window(WindowCmd, Option<usize>),
    /// The key was consumed as part of an in-progress sequence (a count
    /// digit, or a prefix awaiting its next character); no motion yet.
    Pending,
    /// The key isn't part of any recognized motion sequence. Any
    /// in-progress count/prefix is discarded, matching vim's behavior of
    /// dropping a pending command on an invalid continuation.
    None,
}

/// `<C-w>{cmd}`: the same single-letter window shortcuts the shell's own
/// `window` command exposes (see exec.rs's `run_window`), minus the size
/// commands (`+`/`-`/`size`) and `fg <id>` (needs an argument beyond a
/// single letter+count) -- matching plan.md's own scoping for this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCmd {
    Next,
    Previous,
    New,
    Close,
    Split,
    VSplit,
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    Balance,
    /// `<C-w>gg` / `<C-w>{N}gg`: go to the first tab, or tab N.
    GotoFirstWindow,
    /// `<C-w>G` / `<C-w>{N}G`: go to the last tab, or tab N.
    GotoLastWindow,
}

#[derive(Debug, Clone)]
enum Pending {
    None,
    G,
    FindChar { till: bool, forward: bool },
    Mark,
    GotoMarkExact,
    GotoMarkLine,
    Z,
    Window,
    // <C-w>g -- awaiting the second 'g' of <C-w>gg, mirroring plain `gg`'s
    // own two-key shape one level under the window leader.
    WindowG,
    Search { forward: bool, text: String },
}

#[derive(Debug, Clone, Copy)]
enum LastSearch {
    Pattern { forward: bool },
    Word { forward: bool },
}

// A short display label for keys `feed` might reasonably want to echo
// into `current_input`'s transcript -- `None` for keys with no natural
// short label (Delete, AltLeft/Right/Up, ...), which just don't
// contribute to the transcript rather than being an error.
fn key_label(key: Key) -> Option<String> {
    Some(match key {
        Key::Char(c) => c.to_string(),
        Key::Left => "\u{2190}".to_string(),
        Key::Right => "\u{2192}".to_string(),
        Key::Up => "\u{2191}".to_string(),
        Key::Down => "\u{2193}".to_string(),
        Key::Home => "Home".to_string(),
        Key::End => "End".to_string(),
        Key::Enter => "<CR>".to_string(),
        Key::Backspace => "<BS>".to_string(),
        Key::Escape => "<Esc>".to_string(),
        Key::CtrlD => "^D".to_string(),
        Key::CtrlU => "^U".to_string(),
        Key::CtrlF => "^F".to_string(),
        Key::CtrlB => "^B".to_string(),
        Key::CtrlE => "^E".to_string(),
        Key::CtrlY => "^Y".to_string(),
        Key::CtrlW => "^W".to_string(),
        _ => return None,
    })
}

pub struct VimKeys {
    count: Option<usize>,
    pending: Pending,
    last_find: Option<(char, bool, bool)>, // (ch, till, forward)
    last_search_text: String,
    last_search: Option<LastSearch>,
    // A human-readable transcript of the keys fed since the last resolved
    // motion (or the last aborted sequence) -- e.g. "20g" while typing
    // `20gg`, or "/cher" while typing a search. Exists purely for a
    // frontend's status-bar display (see repl.rs's normal-mode status
    // bar); has no effect on how keys are interpreted.
    current_input: String,
    // A snapshot of `current_input` taken at the moment it last resolved
    // into a motion -- e.g. "20gg" -- kept around (not cleared) until the
    // next key starts a new sequence, so a frontend can flash "here's what
    // that just did" for a beat after the motion applies.
    last_completed: String,
}

impl VimKeys {
    pub fn new() -> Self {
        VimKeys {
            count: None,
            pending: Pending::None,
            last_find: None,
            last_search_text: String::new(),
            last_search: None,
            current_input: String::new(),
            last_completed: String::new(),
        }
    }

    /// What's been typed so far toward the motion/search/command in
    /// progress -- empty when nothing is pending. Display only.
    pub fn pending_display(&self) -> &str {
        &self.current_input
    }

    /// The keys that produced the most recently applied motion -- stays
    /// populated (for a frontend to flash briefly) until the next key
    /// starts a new sequence. Display only.
    pub fn last_motion_display(&self) -> &str {
        &self.last_completed
    }

    pub fn feed(&mut self, key: Key) -> KeyOutcome {
        if let Some(label) = key_label(key) {
            self.current_input.push_str(&label);
        }
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::None => self.feed_fresh(key),
            Pending::G => self.feed_g(key),
            Pending::FindChar { till, forward } => self.feed_find_char(key, till, forward),
            Pending::Mark => self.feed_mark(key, MarkKind::Set),
            Pending::GotoMarkExact => self.feed_mark(key, MarkKind::GotoExact),
            Pending::GotoMarkLine => self.feed_mark(key, MarkKind::GotoLine),
            Pending::Z => self.feed_z(key),
            Pending::Window => self.feed_window(key),
            Pending::WindowG => self.feed_window_g(key),
            Pending::Search { forward, text } => self.feed_search(key, forward, text),
        }
    }

    fn emit(&mut self, motion: Motion) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Motion(motion, count)
    }

    fn emit_window(&mut self, cmd: WindowCmd) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Window(cmd, count)
    }

    fn abort(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.current_input.clear();
        KeyOutcome::None
    }

    fn feed_fresh(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char(c) if c.is_ascii_digit() => {
                if c == '0' && self.count.is_none() {
                    return self.emit(Motion::LineStart);
                }
                let d = (c as u8 - b'0') as usize;
                self.count = Some(self.count.unwrap_or(0) * 10 + d);
                KeyOutcome::Pending
            }
            Key::Char('h') | Key::Left => self.emit(Motion::Left),
            Key::Char('l') | Key::Right => self.emit(Motion::Right),
            Key::Char('j') | Key::Down => self.emit(Motion::Down),
            Key::Char('k') | Key::Up => self.emit(Motion::Up),
            Key::Char('^') => self.emit(Motion::LineFirstNonBlank),
            Key::Char('$') | Key::End => self.emit(Motion::LineEnd),
            Key::Home => self.emit(Motion::LineStart),
            Key::Char('|') => self.emit(Motion::GotoColumn),
            Key::Char('G') => self.emit(Motion::GotoLastLine),
            Key::Char('w') => self.emit(Motion::WordForward),
            Key::Char('W') => self.emit(Motion::WordForwardBig),
            Key::Char('b') => self.emit(Motion::WordBackward),
            Key::Char('B') => self.emit(Motion::WordBackwardBig),
            Key::Char('e') => self.emit(Motion::WordEnd),
            Key::Char('E') => self.emit(Motion::WordEndBig),
            Key::Char('f') => {
                self.pending = Pending::FindChar { till: false, forward: true };
                KeyOutcome::Pending
            }
            Key::Char('F') => {
                self.pending = Pending::FindChar { till: false, forward: false };
                KeyOutcome::Pending
            }
            Key::Char('t') => {
                self.pending = Pending::FindChar { till: true, forward: true };
                KeyOutcome::Pending
            }
            Key::Char('T') => {
                self.pending = Pending::FindChar { till: true, forward: false };
                KeyOutcome::Pending
            }
            Key::Char(';') => self.emit_last_find(true),
            Key::Char(',') => self.emit_last_find(false),
            Key::Char('H') => self.emit(Motion::ScreenTop),
            Key::Char('M') => self.emit(Motion::ScreenMiddle),
            Key::Char('L') => self.emit(Motion::ScreenBottom),
            Key::CtrlD => self.emit(Motion::HalfPageDown),
            Key::CtrlU => self.emit(Motion::HalfPageUp),
            Key::CtrlF => self.emit(Motion::PageDown),
            Key::CtrlB => self.emit(Motion::PageUp),
            Key::CtrlE => self.emit(Motion::ScrollLineDown),
            Key::CtrlY => self.emit(Motion::ScrollLineUp),
            Key::Char('{') => self.emit(Motion::ParagraphBackward),
            Key::Char('}') => self.emit(Motion::ParagraphForward),
            Key::Char('(') => self.emit(Motion::SentenceBackward),
            Key::Char(')') => self.emit(Motion::SentenceForward),
            Key::Char('+') | Key::Enter => self.emit(Motion::NextLineNonBlank),
            Key::Char('-') => self.emit(Motion::PrevLineNonBlank),
            Key::Char('%') => self.emit(Motion::MatchPair),
            Key::Char('m') => {
                self.pending = Pending::Mark;
                KeyOutcome::Pending
            }
            Key::Char('`') => {
                self.pending = Pending::GotoMarkExact;
                KeyOutcome::Pending
            }
            Key::Char('\'') => {
                self.pending = Pending::GotoMarkLine;
                KeyOutcome::Pending
            }
            Key::Char('/') => {
                self.pending = Pending::Search { forward: true, text: String::new() };
                KeyOutcome::Pending
            }
            Key::Char('?') => {
                self.pending = Pending::Search { forward: false, text: String::new() };
                KeyOutcome::Pending
            }
            Key::Char('n') => self.emit_last_search(true),
            Key::Char('N') => self.emit_last_search(false),
            Key::Char('*') => {
                self.last_search = Some(LastSearch::Word { forward: true });
                self.emit(Motion::SearchWordForward)
            }
            Key::Char('#') => {
                self.last_search = Some(LastSearch::Word { forward: false });
                self.emit(Motion::SearchWordBackward)
            }
            Key::Char('z') => {
                self.pending = Pending::Z;
                KeyOutcome::Pending
            }
            Key::Char('g') => {
                self.pending = Pending::G;
                KeyOutcome::Pending
            }
            Key::CtrlW => {
                self.pending = Pending::Window;
                KeyOutcome::Pending
            }
            _ => self.abort(),
        }
    }

    fn feed_g(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char('g') => self.emit(Motion::GotoFirstLine),
            Key::Char('_') => self.emit(Motion::LineLastNonBlank),
            Key::Char('e') => self.emit(Motion::WordEndBackward),
            Key::Char('E') => self.emit(Motion::WordEndBackwardBig),
            _ => self.abort(),
        }
    }

    fn feed_find_char(&mut self, key: Key, till: bool, forward: bool) -> KeyOutcome {
        match key {
            Key::Char(c) => {
                self.last_find = Some((c, till, forward));
                self.emit(Motion::FindChar { ch: c, till, forward })
            }
            _ => self.abort(),
        }
    }

    fn feed_mark(&mut self, key: Key, kind: MarkKind) -> KeyOutcome {
        match key {
            Key::Char(c) if c.is_ascii_lowercase() => self.emit(match kind {
                MarkKind::Set => Motion::SetMark(c),
                MarkKind::GotoExact => Motion::GotoMark(c),
                MarkKind::GotoLine => Motion::GotoMarkLine(c),
            }),
            _ => self.abort(),
        }
    }

    fn feed_z(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char('z') => self.emit(Motion::ScrollCenter),
            Key::Char('t') => self.emit(Motion::ScrollTop),
            Key::Char('b') => self.emit(Motion::ScrollBottom),
            _ => self.abort(),
        }
    }

    fn feed_window(&mut self, key: Key) -> KeyOutcome {
        match key {
            // Unlike a bare `<C-w>`-less count (accumulated in feed_fresh,
            // which only applies to *repeating* a command), digits typed
            // here -- inside the leader, before its resolving key -- are
            // for `<C-w>{N}gg`/`<C-w>{N}G`'s absolute tab position. Stays
            // pending, mirroring feed_fresh's own digit arm.
            Key::Char(c) if c.is_ascii_digit() => {
                let d = (c as u8 - b'0') as usize;
                self.count = Some(self.count.unwrap_or(0) * 10 + d);
                self.pending = Pending::Window;
                KeyOutcome::Pending
            }
            Key::Char('n') => self.emit_window(WindowCmd::Next),
            Key::Char('p') => self.emit_window(WindowCmd::Previous),
            Key::Char('c') => self.emit_window(WindowCmd::New),
            Key::Char('q') => self.emit_window(WindowCmd::Close),
            Key::Char('s') => self.emit_window(WindowCmd::Split),
            Key::Char('v') => self.emit_window(WindowCmd::VSplit),
            Key::Char('h') => self.emit_window(WindowCmd::FocusLeft),
            Key::Char('j') => self.emit_window(WindowCmd::FocusDown),
            Key::Char('k') => self.emit_window(WindowCmd::FocusUp),
            Key::Char('l') => self.emit_window(WindowCmd::FocusRight),
            Key::Char('=') => self.emit_window(WindowCmd::Balance),
            Key::Char('g') => {
                self.pending = Pending::WindowG;
                KeyOutcome::Pending
            }
            Key::Char('G') => self.emit_window(WindowCmd::GotoLastWindow),
            _ => self.abort(),
        }
    }

    fn feed_window_g(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char('g') => self.emit_window(WindowCmd::GotoFirstWindow),
            _ => self.abort(),
        }
    }

    fn feed_search(&mut self, key: Key, forward: bool, mut text: String) -> KeyOutcome {
        match key {
            Key::Enter => {
                self.last_search_text = text.clone();
                self.last_search = Some(LastSearch::Pattern { forward });
                self.emit(if forward {
                    Motion::SearchForward(text)
                } else {
                    Motion::SearchBackward(text)
                })
            }
            Key::Escape => self.abort(),
            Key::Backspace => {
                text.pop();
                self.pending = Pending::Search { forward, text };
                KeyOutcome::Pending
            }
            Key::Char(c) => {
                text.push(c);
                self.pending = Pending::Search { forward, text };
                KeyOutcome::Pending
            }
            _ => {
                // Ignore anything else while typing a search string rather
                // than aborting it -- a stray unrecognized key shouldn't
                // discard what's already been typed.
                self.pending = Pending::Search { forward, text };
                KeyOutcome::Pending
            }
        }
    }

    fn emit_last_find(&mut self, same_direction: bool) -> KeyOutcome {
        match self.last_find {
            Some((ch, till, forward)) => {
                let forward = if same_direction { forward } else { !forward };
                self.emit(Motion::FindChar { ch, till, forward })
            }
            None => self.abort(),
        }
    }

    fn emit_last_search(&mut self, same_direction: bool) -> KeyOutcome {
        match self.last_search {
            Some(LastSearch::Pattern { forward }) => {
                let forward = if same_direction { forward } else { !forward };
                let text = self.last_search_text.clone();
                self.emit(if forward {
                    Motion::SearchForward(text)
                } else {
                    Motion::SearchBackward(text)
                })
            }
            Some(LastSearch::Word { forward }) => {
                let forward = if same_direction { forward } else { !forward };
                self.emit(if forward {
                    Motion::SearchWordForward
                } else {
                    Motion::SearchWordBackward
                })
            }
            None => self.abort(),
        }
    }
}

impl Default for VimKeys {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
enum MarkKind {
    Set,
    GotoExact,
    GotoLine,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(vk: &mut VimKeys, keys: &[Key]) -> Vec<KeyOutcome> {
        keys.iter().cloned().map(|k| vk.feed(k)).collect()
    }

    fn last(vk: &mut VimKeys, keys: &[Key]) -> KeyOutcome {
        feed_all(vk, keys).pop().unwrap()
    }

    #[test]
    fn simple_single_key_motions() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('h')), KeyOutcome::Motion(Motion::Left, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('0')), KeyOutcome::Motion(Motion::LineStart, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('%')), KeyOutcome::Motion(Motion::MatchPair, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('$')), KeyOutcome::Motion(Motion::LineEnd, None));
    }

    #[test]
    fn arrow_and_home_end_alias_to_motions() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Left), KeyOutcome::Motion(Motion::Left, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Right), KeyOutcome::Motion(Motion::Right, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Up), KeyOutcome::Motion(Motion::Up, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Down), KeyOutcome::Motion(Motion::Down, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Home), KeyOutcome::Motion(Motion::LineStart, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::End), KeyOutcome::Motion(Motion::LineEnd, None));
    }

    #[test]
    fn ctrl_keys_map_to_screen_motions() {
        let cases = [
            (Key::CtrlD, Motion::HalfPageDown),
            (Key::CtrlU, Motion::HalfPageUp),
            (Key::CtrlF, Motion::PageDown),
            (Key::CtrlB, Motion::PageUp),
            (Key::CtrlE, Motion::ScrollLineDown),
            (Key::CtrlY, Motion::ScrollLineUp),
        ];
        for (key, motion) in cases {
            let mut vk = VimKeys::new();
            assert_eq!(vk.feed(key), KeyOutcome::Motion(motion, None));
        }
    }

    #[test]
    fn count_accumulates_across_digits() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('3')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, Some(3)));

        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('1')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('0')), KeyOutcome::Pending); // '0' after a digit isn't LineStart
        assert_eq!(vk.feed(Key::Char('j')), KeyOutcome::Motion(Motion::Down, Some(10)));
    }

    #[test]
    fn g_prefix_motions() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Motion(Motion::GotoFirstLine, None));

        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('3'), Key::Char('g'), Key::Char('g')]),
            KeyOutcome::Motion(Motion::GotoFirstLine, Some(3))
        );

        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('g'), Key::Char('_')]),
            KeyOutcome::Motion(Motion::LineLastNonBlank, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('g'), Key::Char('e')]),
            KeyOutcome::Motion(Motion::WordEndBackward, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('g'), Key::Char('E')]),
            KeyOutcome::Motion(Motion::WordEndBackwardBig, None)
        );
    }

    #[test]
    fn unrecognized_continuation_aborts_pending_and_count() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('3')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::None); // 'gx' isn't a thing
        // the aborted count/prefix must not leak into the next command
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn find_char_and_repeat() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('f')), KeyOutcome::Pending);
        assert_eq!(
            vk.feed(Key::Char('x')),
            KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, None)
        );
        // ';' repeats the same direction
        assert_eq!(
            vk.feed(Key::Char(';')),
            KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, None)
        );
        // ',' repeats with direction flipped
        assert_eq!(
            vk.feed(Key::Char(',')),
            KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: false }, None)
        );
    }

    #[test]
    fn till_and_backward_find_char() {
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('t'), Key::Char('y')]),
            KeyOutcome::Motion(Motion::FindChar { ch: 'y', till: true, forward: true }, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('T'), Key::Char('y')]),
            KeyOutcome::Motion(Motion::FindChar { ch: 'y', till: true, forward: false }, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('F'), Key::Char('y')]),
            KeyOutcome::Motion(Motion::FindChar { ch: 'y', till: false, forward: false }, None)
        );
    }

    #[test]
    fn semicolon_with_no_prior_find_is_a_no_op() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char(';')), KeyOutcome::None);
    }

    #[test]
    fn marks() {
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('m'), Key::Char('a')]),
            KeyOutcome::Motion(Motion::SetMark('a'), None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('`'), Key::Char('a')]),
            KeyOutcome::Motion(Motion::GotoMark('a'), None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('\''), Key::Char('a')]),
            KeyOutcome::Motion(Motion::GotoMarkLine('a'), None)
        );
    }

    #[test]
    fn z_prefix_scroll_motions() {
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('z'), Key::Char('z')]),
            KeyOutcome::Motion(Motion::ScrollCenter, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('z'), Key::Char('t')]),
            KeyOutcome::Motion(Motion::ScrollTop, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('z'), Key::Char('b')]),
            KeyOutcome::Motion(Motion::ScrollBottom, None)
        );
    }

    #[test]
    fn search_forward_and_repeat() {
        let mut vk = VimKeys::new();
        let keys = [
            Key::Char('/'),
            Key::Char('f'),
            Key::Char('o'),
            Key::Char('o'),
            Key::Enter,
        ];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Motion(Motion::SearchForward("foo".to_string()), None)
        );
        assert_eq!(
            vk.feed(Key::Char('n')),
            KeyOutcome::Motion(Motion::SearchForward("foo".to_string()), None)
        );
        assert_eq!(
            vk.feed(Key::Char('N')),
            KeyOutcome::Motion(Motion::SearchBackward("foo".to_string()), None)
        );
    }

    #[test]
    fn search_backward_and_backspace_editing() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('?')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('b')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('a')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('z')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Backspace), KeyOutcome::Pending); // "ba"
        assert_eq!(vk.feed(Key::Char('r')), KeyOutcome::Pending); // "bar"
        assert_eq!(
            vk.feed(Key::Enter),
            KeyOutcome::Motion(Motion::SearchBackward("bar".to_string()), None)
        );
    }

    #[test]
    fn search_escape_cancels() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('/')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Escape), KeyOutcome::None);
        // back to a clean state afterward
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn star_and_hash_word_search_with_repeat() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('*')), KeyOutcome::Motion(Motion::SearchWordForward, None));
        assert_eq!(vk.feed(Key::Char('n')), KeyOutcome::Motion(Motion::SearchWordForward, None));
        assert_eq!(vk.feed(Key::Char('N')), KeyOutcome::Motion(Motion::SearchWordBackward, None));

        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('#')), KeyOutcome::Motion(Motion::SearchWordBackward, None));
    }

    #[test]
    fn n_with_no_prior_search_is_a_no_op() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('n')), KeyOutcome::None);
    }

    #[test]
    fn paragraph_and_sentence_and_line_motions() {
        let cases = [
            ('{', Motion::ParagraphBackward),
            ('}', Motion::ParagraphForward),
            ('(', Motion::SentenceBackward),
            (')', Motion::SentenceForward),
            ('+', Motion::NextLineNonBlank),
            ('-', Motion::PrevLineNonBlank),
            ('^', Motion::LineFirstNonBlank),
            ('|', Motion::GotoColumn),
            ('H', Motion::ScreenTop),
            ('M', Motion::ScreenMiddle),
            ('L', Motion::ScreenBottom),
            ('G', Motion::GotoLastLine),
        ];
        for (ch, motion) in cases {
            let mut vk = VimKeys::new();
            assert_eq!(vk.feed(Key::Char(ch)), KeyOutcome::Motion(motion, None));
        }
    }

    #[test]
    fn enter_is_next_line_non_blank() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Enter), KeyOutcome::Motion(Motion::NextLineNonBlank, None));
    }

    #[test]
    fn count_survives_into_find_char_and_search() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('f'), Key::Char('x')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, Some(2))
        );
    }

    #[test]
    fn pending_display_shows_count_and_prefix_as_typed() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.pending_display(), "");
        vk.feed(Key::Char('2'));
        assert_eq!(vk.pending_display(), "2");
        vk.feed(Key::Char('0'));
        assert_eq!(vk.pending_display(), "20");
        vk.feed(Key::Char('g'));
        assert_eq!(vk.pending_display(), "20g");
        vk.feed(Key::Char('g'));
        // resolved into a motion -- nothing pending anymore
        assert_eq!(vk.pending_display(), "");
    }

    #[test]
    fn last_motion_display_flashes_the_completed_sequence() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.last_motion_display(), "");
        vk.feed(Key::Char('2'));
        vk.feed(Key::Char('0'));
        vk.feed(Key::Char('k'));
        assert_eq!(vk.last_motion_display(), "20k");
        // stays until the next sequence starts resolving
        vk.feed(Key::Char('j'));
        assert_eq!(vk.last_motion_display(), "j");
    }

    #[test]
    fn aborted_sequence_clears_pending_but_not_the_last_flash() {
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('h'));
        assert_eq!(vk.last_motion_display(), "h");
        vk.feed(Key::Char('g'));
        vk.feed(Key::Char('x')); // 'gx' isn't a thing -- aborts
        assert_eq!(vk.pending_display(), "");
        assert_eq!(vk.last_motion_display(), "h"); // unchanged by the abort
    }

    #[test]
    fn search_pending_display_shows_the_slash_and_typed_text() {
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('/'));
        assert_eq!(vk.pending_display(), "/");
        vk.feed(Key::Char('f'));
        vk.feed(Key::Char('o'));
        vk.feed(Key::Char('o'));
        assert_eq!(vk.pending_display(), "/foo");
        vk.feed(Key::Enter);
        assert_eq!(vk.pending_display(), "");
        assert_eq!(vk.last_motion_display(), "/foo<CR>");
    }

    #[test]
    fn window_leader_commands() {
        let cases = [
            ('n', WindowCmd::Next),
            ('p', WindowCmd::Previous),
            ('c', WindowCmd::New),
            ('q', WindowCmd::Close),
            ('s', WindowCmd::Split),
            ('v', WindowCmd::VSplit),
            ('h', WindowCmd::FocusLeft),
            ('j', WindowCmd::FocusDown),
            ('k', WindowCmd::FocusUp),
            ('l', WindowCmd::FocusRight),
            ('=', WindowCmd::Balance),
        ];
        for (ch, cmd) in cases {
            let mut vk = VimKeys::new();
            assert_eq!(vk.feed(Key::CtrlW), KeyOutcome::Pending);
            assert_eq!(vk.feed(Key::Char(ch)), KeyOutcome::Window(cmd, None));
        }
    }

    #[test]
    fn window_leader_command_with_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::CtrlW, Key::Char('n')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::Next, Some(2)));
    }

    #[test]
    fn window_leader_unrecognized_continuation_aborts() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlW), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::None);
        // aborted cleanly -- next key starts fresh
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn window_goto_first_and_last_bare() {
        let mut vk = VimKeys::new();
        let keys = [Key::CtrlW, Key::Char('g'), Key::Char('g')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::GotoFirstWindow, None));

        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlW), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('G')), KeyOutcome::Window(WindowCmd::GotoLastWindow, None));
    }

    #[test]
    fn window_goto_nth_tab() {
        let mut vk = VimKeys::new();
        let keys = [Key::CtrlW, Key::Char('5'), Key::Char('g'), Key::Char('g')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::GotoFirstWindow, Some(5)));

        let mut vk = VimKeys::new();
        let keys = [Key::CtrlW, Key::Char('5'), Key::Char('G')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::GotoLastWindow, Some(5)));
    }

    #[test]
    fn window_goto_nth_tab_multi_digit_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::CtrlW, Key::Char('1'), Key::Char('2'), Key::Char('G')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::GotoLastWindow, Some(12)));
    }

    #[test]
    fn window_g_unrecognized_continuation_aborts() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlW), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::None); // '<C-w>gx' isn't a thing
        assert_eq!(vk.feed(Key::Char('h')), KeyOutcome::Motion(Motion::Left, None));
    }

    #[test]
    fn window_pending_display_shows_the_leader_and_count() {
        let mut vk = VimKeys::new();
        vk.feed(Key::CtrlW);
        assert_eq!(vk.pending_display(), "^W");
        vk.feed(Key::Char('5'));
        assert_eq!(vk.pending_display(), "^W5");
        vk.feed(Key::Char('g'));
        assert_eq!(vk.pending_display(), "^W5g");
        vk.feed(Key::Char('g'));
        assert_eq!(vk.pending_display(), "");
        assert_eq!(vk.last_motion_display(), "^W5gg");
    }
}
