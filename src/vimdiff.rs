// A corpus of editor keystrokes that vim and bish's own editor should
// agree on, run through both and diffed.
//
// The same idea as `bashdiff.rs`, one layer up: bish's editor is
// vim-like, vim is the reference, and the two can be driven the same
// way. Adding a case costs one line, so the next question of the form
// "does bish's `dt,` do what vim's does?" is answered by writing it
// down.
//
// What is compared is the *file*, never the screen. Both editors are
// given the same starting content, the same keystrokes and then `:wq`,
// and the resulting bytes on disk are diffed -- so nothing here depends
// on rendering, colours, or where either editor puts the cursor.
//
// Two details that are not obvious and cost a real finding each:
//
// - Keys go in one at a time. `dt,` arriving in a single read is a
//   different thing from a person typing it, and one of the first three
//   differences this found went away once they were separated.
// - vim is run as `vim -u NONE -i NONE -N`: no vimrc, no viminfo, and
//   nocompatible. Without `-u NONE` the corpus measures the local
//   configuration instead of vim.
//
// Skipped where there is no vim, the way the bash corpus skips without
// bash.

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct Case {
        name: &'static str,
        /// What the file holds before the keys are sent.
        before: &'static str,
        /// Sent one character at a time, in order.
        keys: &'static str,
    }

    const fn case(name: &'static str, before: &'static str, keys: &'static str) -> Case {
        Case { name, before, keys }
    }

    // `\u{1b}` is escape and `\r` is Enter; both are ordinary keys here.
    const CASES: &[Case] = &[
        // -- deleting --------------------------------------------------
        case("dd", "one\ntwo\nthree\n", "dd"),
        case("cc", "abc\ndef\n", "ccZ\u{1b}"),
        case("cc-with-count", "a\nb\nc\n", "2ccZ\u{1b}"),
        case("r", "abc\n", "rZ"),
        case("r-with-count", "abcdef\n", "3rZ"),
        // `Z` is `ZZ`'s own first key, intercepted ahead of the key
        // machine -- so `rZ` is the case that catches an interception
        // that forgot to check whether anything was pending.
        case("ZZ-still-saves", "one\ntwo\n", "xZZ"),
        case("t-motion", "a,b,c\n", "dt,"),
        case("t-motion-mid-line", "abcdef\n", "dtc"),
        case("T-motion-backward", "a,b\n", "$dT,"),
        case("visual-o", "abcdef\n", "llvohd"),
        case("visual-big-o", "abc\n", "vOx"),
        case("visual-o-linewise", "a\nb\nc\nd\n", "jVjokd"),
        case("dot-repeats", "aaa bbb ccc\n", "dw."),
        case("dot-repeats-twice", "abcdef\n", "x.."),
        case("dot-repeats-an-insert", "a\nb\n", "IX\u{1b}j."),
        case("dot-repeats-a-change", "one two three\n", "cwZ\u{1b}w."),
        case("dot-repeats-a-line-delete", "a\nb\nc\nd\n", "dd."),
        case("dot-after-undo-repeats-the-change-not-the-undo", "abcdef\n", "xu."),
        case("dot-with-nothing-to-repeat", "abc\n", "."),
        case("mark-linewise-delete", "a\nb\nc\n", "maGd'a"),
        case("mark-linewise-delete-partial", "a\nb\nc\nd\n", "jmaGd'a"),
        // An empty file and a file of one empty line are different
        // files, and a buffer that always keeps a line to put the
        // cursor on has to remember which it started as.
        case("empty-file-stays-empty", "", "0"),
        case("one-newline-file-stays-one-newline", "\n", "0"),
        case("deleting-every-line-empties-the-file", "a\nb\nc\n", "dG"),
        case("emptying-the-only-line-keeps-its-newline", "\n", "iX\u{1b}x"),
        case("ex-line-address", "a\nb\nc\n", ":2\rdd"),
        case("ex-line-address-last", "a\nb\nc\n", ":$\rdd"),
        case("ex-delete-a-line", "one\ntwo\nthree\n", ":2d\r"),
        case("ex-delete-a-range", "a\nb\nc\nd\n", ":2,3d\r"),
        case("ex-move-a-line", "one\ntwo\n", ":1m$\r"),
        case("ex-move-to-the-top", "one\ntwo\nthree\n", ":3m0\r"),
        case("ex-move-a-range", "a\nb\nc\nd\n", ":1,2m$\r"),
        case("ex-move-into-itself-is-refused", "a\nb\nc\n", ":1,2m1\r"),
        case("ex-normal", "abc\n", ":normal x\r"),
        case("ex-normal-runs-a-whole-command", "a\nb\nc\n", ":normal dd\r"),
        case("dd-with-count", "one\ntwo\nthree\n", "2dd"),
        case("dw", "alpha beta gamma\n", "dw"),
        case("dw-with-count", "a b c d e\n", "d3w"),
        case("de", "alpha beta\n", "de"),
        case("x", "abc\n", "x"),
        case("x-with-count", "abcdef\n", "3x"),
        case("x-at-end-of-line", "ab\n", "$xxx"),
        case("D", "abcdef\n", "llD"),
        case("dj", "a\nb\nc\n", "dj"),
        case("dk", "a\nb\nc\n", "jdk"),
        case("dG", "a\nb\nc\n", "jdG"),
        case("dgg", "a\nb\nc\n", "jdgg"),
        case("d-caret", "  abc\n", "$d^"),
        case("dW", "a.b c.d\n", "dW"),
        // -- inserting -------------------------------------------------
        case("A", "abc\n", "AXY\u{1b}"),
        case("A-on-an-empty-line", "\n", "Ahi\u{1b}"),
        case("I", "abc\n", "IXY\u{1b}"),
        case("o", "abc\n", "onew\u{1b}"),
        case("O", "abc\n", "Onew\u{1b}"),
        case("a-at-end-of-line", "abc\n", "$aZ\u{1b}"),
        case("i-at-start", "abc\n", "$0iZ\u{1b}"),
        case("e-then-a", "ab cd\n", "eaX\u{1b}"),
        // -- changing --------------------------------------------------
        case("cw", "alpha beta\n", "cwX\u{1b}"),
        case("ciw", "alpha beta\n", "wciwX\u{1b}"),
        case("caw", "alpha beta gamma\n", "wcawX\u{1b}"),
        case("C", "abcdef\n", "llCZ\u{1b}"),
        case("s", "abc\n", "sZ\u{1b}"),
        case("S", "abc\ndef\n", "SZ\u{1b}"),
        case("R", "abcdef\n", "RXY\u{1b}"),
        case("tilde", "abc\n", "~~"),
        case("tilde-with-count", "abcdef\n", "3~"),
        case("gUiw", "abc def\n", "gUiw"),
        case("guiw", "ABC DEF\n", "guiw"),
        case("J", "one\ntwo\n", "J"),
        case("J-with-count", "a\nb\nc\n", "3J"),
        case("gJ", "one\n  two\n", "gJ"),
        // -- text objects ----------------------------------------------
        case("di-paren", "f(a b)c\n", "f(di("),
        case("da-bracket", "x [a b] y\n", "f[da["),
        case("ci-quote", "say \"hi there\" ok\n", "f\"ci\"Z\u{1b}"),
        case("dip", "a\nb\n\nc\n", "dip"),
        case("dap", "a\nb\n\nc\n", "dap"),
        // -- motions ---------------------------------------------------
        case("f-motion", "a,b,c\n", "df,"),
        case("F-motion", "a,b,c\n", "$dF,"),
        case("T-motion", "a,b,c\n", "$dT,"),
        case("semicolon-repeats", "a,b,c,d\n", "f,;x"),
        case("comma-reverses", "a,b,c,d\n", "f,f,,x"),
        case("percent-paren", "(abc)\n", "%x"),
        case("percent-brace", "{a}\n", "%x"),
        case("w-with-count", "aa bb cc\n", "2wx"),
        case("ge", "ab cd\n", "$dge"),
        case("brace-forward", "a\n\nb\n", "d}"),
        case("G-then-dd", "a\nb\nc\n", "Gdd"),
        case("gg-then-dd", "a\nb\nc\n", "Gggdd"),
        case("L-then-dd", "a\nb\nc\n", "Ldd"),
        // -- yank and put ----------------------------------------------
        case("yyp", "one\ntwo\n", "yyp"),
        case("yyP", "one\n", "yyP"),
        case("yw-then-p", "ab cd\n", "yw$p"),
        case("yiw-then-P", "abc def\n", "yiwwP"),
        case("p-after-dd", "one\ntwo\nthree\n", "ddp"),
        case("P-before", "one\ntwo\n", "yyjP"),
        case("D-then-p", "abcdef\n", "llD0p"),
        case("named-register", "one\ntwo\n", "\"ayyj\"ap"),
        case("yank-register-zero", "one\ntwo\n", "yyjyy\"0p"),
        case("black-hole-register", "one\ntwo\n", "yyj\"_ddp"),
        // -- visual ----------------------------------------------------
        case("visual-d", "abcdef\n", "vlld"),
        case("visual-line-d", "one\ntwo\n", "Vd"),
        case("visual-block-d", "ab\ncd\n", "\u{16}jd"),
        case("gv", "abcdef\n", "vl\u{1b}gvd"),
        // -- undo and repeat -------------------------------------------
        case("u", "abc\n", "xu"),
        case("u-twice", "abc\n", "xxuu"),
        case("ctrl-r", "abc\n", "xu\u{12}"),
        case("redo-twice", "abcd\n", "xxuu\u{12}\u{12}"),
        case("undo-a-change-as-one", "abc\n", "cwZ\u{1b}u"),
        // -- marks, search, registers ----------------------------------
        case("search-then-x", "aXa\nbXb\n", "/X\rx"),
        case("search-backwards", "aXbXc\n", "$?X\rx"),
        case("search-next", "aXbXcX\n", "/X\rnx"),
        case("star-search", "foo bar\nfoo baz\n", "*x"),
        // -- macros ----------------------------------------------------
        case("macro-record-and-play", "a\nb\nc\n", "qaxjq@a"),
        case("macro-with-count", "aa\nbb\ncc\n", "qaxjq2@a"),
        // -- ex commands -----------------------------------------------
        case("ex-substitute", "foo bar\n", ":s/foo/baz/\r"),
        case("ex-substitute-global", "a a a\n", ":s/a/b/g\r"),
        case("ex-substitute-whole-file", "a\na\na\n", ":%s/a/b/\r"),
        case("ex-global-delete", "a1\nb2\na3\n", ":g/a/d\r"),
        // -- arithmetic on the buffer ----------------------------------
        case("ctrl-a", "x 41 y\n", "\u{1}"),
        case("ctrl-x", "x 41 y\n", "\u{18}"),
        // -- roadmap 05 item 8: a sweep of untouched editor ground ----
        // Joining, and the shorthand operators -- the ones that are a
        // whole command in one key.
        case("join-lines", "a\nb\n", "J"),
        case("join-with-count", "a\nb\nc\n", "3J"),
        case("join-without-a-space", "a\nb\n", "gJ"),
        case("capital-d", "abc\n", "lD"),
        case("capital-c", "abc\n", "lCX\u{1b}"),
        case("capital-s", "abc\ndef\n", "SX\u{1b}"),
        case("capital-y-then-p", "a\nb\n", "Yp"),
        case("capital-p", "a\nb\n", "yyP"),
        case("capital-a", "ab\n", "AX\u{1b}"),
        case("capital-i", "  ab\n", "IX\u{1b}"),
        case("capital-o", "b\n", "OX\u{1b}"),
        case("replace-mode", "abcd\n", "RXY\u{1b}"),
        // The two swaps every vim tutorial teaches.
        case("xp-swaps-two-characters", "ab\n", "xp"),
        case("ddp-swaps-two-lines", "a\nb\n", "ddp"),
        // Word motions that are not `w`.
        case("b-motion", "one two\n", "$bD"),
        case("capital-w-motion", "a.b c\n", "WD"),
        case("capital-e-motion", "a.b c\n", "ED"),
        case("capital-b-motion", "a.b c\n", "$BD"),
        // Motions to a place on the screen rather than in the text.
        case("h-motion", "a\nb\nc\n", "GHdd"),
        case("m-motion", "a\nb\nc\n", "Mdd"),
        case("l-motion", "a\nb\nc\n", "Ldd"),
        case("g-with-a-count", "a\nb\nc\n", "2Gdd"),
        // Column motions.
        case("pipe-motion", "abcdef\n", "4|D"),
        case("caret-motion", "  abc\n", "^D"),
        case("dollar-motion", "abc\n", "0$x"),
        // A brace text object, and a mark used charwise.
        case("di-brace", "x{ab}y\n", "fadi{"),
        case("backtick-mark", "abc\n", "lmax0d`a"),
        // Registers: appending with an uppercase name.
        case("uppercase-register-appends", "a\nb\n", "\"ayyj\"Ayygg\"ap"),
        // Ex forms the corpus had not reached.
        case("ex-copy-a-line", "a\nb\n", ":1t$\r"),
        case("ex-invert-global", "a\nb\na\n", ":v/b/d\r"),
        case("hash-search", "aa\nbb\naa\n", "G#dd"),
        // Editing keys that only exist in insert mode.
        case("ctrl-w-in-insert", "\n", "iab cd\u{17}\u{1b}"),
        case("ctrl-u-in-insert", "\n", "iabc\u{15}X\u{1b}"),
        // Case operators, and a count on a shift.
        case("gu-upper-a-word", "abc\n", "gUiw"),
        case("g-tilde-a-line", "aBc\n", "g~~"),
        case("visual-paste-over", "ab\ncd\n", "yyjVp"),
    ];

    // Cases bish does not match today, each with why. Asserted to
    // *still* differ, so one that gets fixed fails this test until its
    // entry is removed -- the same contract `bashdiff.rs` uses.
    const DIVERGENCES: &[(&str, &str)] = &[
        (
            "indent-width",
            "`>>` inserts four spaces where `vim -u NONE` inserts a tab -- bish reads .editorconfig and defaults to spaces, which is a choice rather than a bug, but it is a difference and it is recorded",
        ),
        ("visual-indent", "indents with spaces where `vim -u NONE` uses a tab -- the same deliberate choice as `indent-width`"),
        // Both of these do the right thing and reach it with the wrong
        // character: the shift happens, on the right lines, in spaces.
        ("ex-shift-right", "`:>` shifts with spaces where `vim -u NONE` uses a tab -- the same choice as `indent-width`"),
        ("shift-right-with-count", "`2>>` shifts both lines, in spaces where `vim -u NONE` uses a tab -- the same choice as `indent-width`"),
        (
            "cc-keeps-the-indent",
            "`cc` leaves the line's indentation and starts the insert past it, where `vim -u NONE` (which has no autoindent) starts at column zero -- bish's `o`/`O` carry the indent too, so this is that same choice rather than a `cc` bug",
        ),
    ];

    const PENDING: &[Case] = &[
        case("indent-width", "a\nb\n", ">>"),
        case("visual-indent", "a\nb\n", "Vj>"),
        case("cc-keeps-the-indent", "  abc\ndef\n", "ccZ\u{1b}"),
        case("ex-shift-right", "a\n", ":>\r"),
        case("shift-right-with-count", "a\nb\n", "2>>"),
    ];

    fn have_vim() -> bool {
        Path::new(VIM).exists()
    }

    const VIM: &str = "/usr/bin/vim";

    /// `target/<profile>/bish`, worked out from the test binary's own
    /// path -- same as `bashdiff.rs`.
    fn bish_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.parent()?.join("bish");
        path.exists().then_some(path)
    }

    /// Runs one editor in a pty over `path`, sends `keys` one character
    /// at a time and then `:wq`, and returns what the file holds
    /// afterwards.
    /// `None` when the editor never answered the handshake, which is a
    /// failure of this harness rather than a difference between the two
    /// editors -- see the call site.
    fn edit(argv: &[String], path: &Path, keys: &str, save: &str) -> Option<String> {
        let Ok(pty) = crate::pty::open() else { return None };
        let _ = crate::pty::set_size(std::os::fd::AsRawFd::as_raw_fd(&pty.master), 24, 80);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.env("TERM", "xterm-256color");
        // No config of either editor's: this measures the editors, not
        // the machine they are on.
        cmd.env_remove("VIMINIT");
        let Ok(mut child) = crate::pty::spawn_attached(cmd, &pty.slave_path) else {
            return None;
        };
        let mut master = pty.master;
        let _ = crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&master));
        // Every read goes through here, and `settle` decides whether
        // the editor has gone quiet from what *this* returns. An
        // earlier version also read directly in `settle` and only
        // counted what was left over, which made "quiet" mean
        // "something else drained it" -- the keys were then sent while
        // the editor was still painting, and it never saw them.
        let mut drain = |master: &mut std::fs::File| -> usize {
            let mut buf = [0u8; 4096];
            let mut total = 0;
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
                total += n;
            }
            total
        };
        // Wait for it to paint its first screen before typing at it.
        // Not just "until quiet": an editor that has not started yet is
        // also quiet, and a key sent before it puts the terminal in raw
        // mode is read by the line discipline instead of by the editor.
        // An editor that never answers is not an editor that did
        // something unexpected. Typing at it anyway produces the file
        // exactly as it was, which is indistinguishable from "the
        // editor ignored every key" -- and that is what a lost
        // handshake used to be reported as.
        // Thirty seconds, not ten. The budget is the one guess left in
        // here about how fast the machine is, and it costs nothing when
        // it is wrong in this direction: the handshake returns the
        // moment it is answered, so a healthy run never waits. A run
        // alongside the rest of the suite, with two editors and a few
        // thousand processes competing, is the case it has to cover.
        if !handshake(&mut master, &mut drain, Duration::from_secs(30)) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        for c in keys.chars().chain(save.chars()) {
            let mut buf = [0u8; 4];
            let _ = master.write_all(c.encode_utf8(&mut buf).as_bytes());
            let floor = if c == '\u{1b}' { ESC_DWELL } else { Duration::from_millis(20) };
            settle(&mut master, &mut drain, floor, KEY_LIMIT, QUIET_TICKS);
        }
        // Wait for it to finish writing, killing it if it will not.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                _ if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                _ => {
                    drain(&mut master);
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        Some(std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>")))
    }

    /// Blocks until the editor has demonstrably read a key, by asking
    /// it a question it must answer on screen: Ctrl-L redraws in both
    /// editors and changes nothing in the buffer.
    ///
    /// Waiting for output instead cannot establish this. An editor
    /// writes several times on its way up and is quiet in the gaps, so
    /// "it has gone quiet" and "it has not started" look identical from
    /// out here -- and a key sent into one of those gaps, before the
    /// terminal is raw, is swallowed by the line discipline rather than
    /// queued. Asking until answered is the only version of this that
    /// does not encode a guess about how fast the machine is: it costs
    /// one poll when the editor is up and waits when it is not.
    ///
    /// The wait for quiet before each ask is the load-bearing part.
    /// Without it the startup paint still arriving counts as the
    /// answer, and the ask is "confirmed" by output that predates it.
    ///
    /// Ctrl-L rather than the more obvious `:`, because `:` has to be
    /// cancelled afterwards and the only key that cancels it is Esc --
    /// which would then sit directly in front of the case's first
    /// keystroke. `Esc` `O` is the SS3 introducer, so the `O` case
    /// lost both its `O` and the `n` after it, and vim recorded a
    /// difference that was entirely the harness's doing. A probe that
    /// needs no cancelling cannot cause that.
    /// Returns whether the editor ever answered. Giving up quietly and
    /// typing anyway is the one thing this must not do: the keys go to
    /// the line discipline, the file comes back untouched, and the case
    /// reports a difference that is entirely this harness's.
    fn handshake(master: &mut std::fs::File, drain: &mut impl FnMut(&mut std::fs::File) -> usize, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            settle(master, drain, Duration::from_millis(150), Duration::from_secs(2), 12);
            let _ = master.write_all(b"\x0c");
            let asked = Instant::now();
            while asked.elapsed() < Duration::from_millis(300) {
                if drain(master) > 0 {
                    settle(master, drain, Duration::from_millis(30), Duration::from_millis(500), QUIET_TICKS);
                    return true;
                }
                std::thread::sleep(Duration::from_millis(4));
            }
        }
        false
    }

    /// How long to leave an escape alone before sending the next key.
    ///
    /// bish decides a lone Esc is a lone Esc by polling for a following
    /// byte for `ESCAPE_TIMEOUT_MS` (30ms, editor.rs) -- and if one
    /// arrives that soon and is not `[` or `O`, the pair decodes to
    /// `Key::Unknown` and *both* bytes are gone. Typed at the speed of
    /// a pty write, `Esc :wq` therefore became nothing at all: the
    /// editor sat there until the harness killed it and the case read
    /// back its unedited file, which is what made every bish result
    /// look like the editor had ignored the whole sequence.
    const ESC_DWELL: Duration = Duration::from_millis(90);

    /// How many consecutive silent 4ms polls mean the editor has
    /// finished responding to a key.
    ///
    /// A redraw is not one burst. bish answers a single keystroke in
    /// several chunks with real gaps between them, so a short run of
    /// silence lands *inside* one keystroke's response and the next key
    /// goes out while the editor is still writing -- which is the
    /// condition ESC_DWELL exists to avoid. This is the number that has
    /// to be generous; the floor only covers keys that draw nothing.
    const QUIET_TICKS: u32 = 40;

    /// How long a single keystroke may take before the next one is sent
    /// anyway.
    ///
    /// Generous on purpose, and for a subtler reason than "give it time
    /// to finish". This clock is what makes ESC_DWELL mean anything.
    /// The dwell has to separate the two bytes *as the editor reads
    /// them*, not as the harness writes them -- and bytes written while
    /// the editor is still redrawing the previous key simply queue up,
    /// so a dwell spent against a busy editor buys no separation at
    /// all. Waiting for the redraw to actually finish before sending
    /// the escape is what keeps the pair apart. A debug-build editor on
    /// a loaded machine can take most of a second on one keystroke.
    const KEY_LIMIT: Duration = Duration::from_millis(2500);

    /// Reads until the editor has produced nothing for `quiet_ticks`
    /// consecutive 4ms polls, or `limit` elapses. Adaptive rather than
    /// a fixed sleep, so a fast keystroke does not pay for a slow one.
    ///
    /// `floor` matters as much as `limit`. A key that changes nothing
    /// on screen produces no output at all, so "quiet" alone cannot
    /// distinguish "done" from "not started"; and an editor still
    /// coming up is quiet in the gaps between its startup writes. Send
    /// a key into one of those gaps -- before it has put the terminal
    /// in raw mode -- and the line discipline eats it. That is exactly
    /// what made this corpus report every bish case as unedited.
    fn settle(master: &mut std::fs::File, drain: &mut impl FnMut(&mut std::fs::File) -> usize, floor: Duration, limit: Duration, quiet_ticks: u32) {
        let started = Instant::now();
        let mut quiet = 0;
        while started.elapsed() < limit && (started.elapsed() < floor || quiet < quiet_ticks) {
            match drain(master) {
                0 => quiet += 1,
                _ => quiet = 0,
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    /// Runs every case through both editors and returns the ones whose
    /// files came out different.
    ///
    /// A case that differs is run again, twice, before it is believed.
    /// Two live editors are being typed at through a pty, and a
    /// keystroke lost to a slow redraw shows up here as "bish did
    /// nothing" -- which looks exactly like a real divergence and is
    /// not one. A genuine difference is deterministic and survives
    /// every rerun; timing noise does not. Twice rather than once
    /// because a single retry still let a flake through when this ran
    /// alongside the rest of the suite, with the machine loaded enough
    /// to lose a key on two attempts running. The reruns cost nothing
    /// on a clean run, since only a differing case pays for them.
    fn compare(cases: &[Case], bish: &Path) -> Vec<(&'static str, String, String)> {
        let root = std::env::temp_dir().join(format!("bish-vimdiff-{}", std::process::id()));
        let mut differing = Vec::new();
        // `BISH_VIMDIFF_ONLY=name` runs just that case. Driving two
        // real editors through a pty takes minutes for the whole
        // corpus, which is a long way to go to look at one case that
        // only misbehaves sometimes.
        let only = std::env::var("BISH_VIMDIFF_ONLY").ok();
        for c in cases {
            if only.as_deref().is_some_and(|want| want != c.name) {
                continue;
            }
            // A directory per case, named after it -- same reasoning as
            // the bash corpus: two tests share this root, and an index
            // would collide.
            let dir = root.join(c.name);
            let path = dir.join("f.txt");
            let run = |argv: Vec<String>, save: &str| -> Option<String> {
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, c.before).unwrap();
                edit(&argv, &path, c.keys, save)
            };
            let want =
                run(vec![VIM.into(), "-u".into(), "NONE".into(), "-i".into(), "NONE".into(), "-N".into(), path.display().to_string()], ":wq\r");
            // bish gets an extra escape first: a case that ends in
            // insert mode would otherwise type `:wq` into the buffer.
            // vim does not need it because `:wq` from insert mode is
            // the same mistake there, and every case here already
            // leaves normal mode.
            let got = run(vec![bish.display().to_string(), "tool".into(), "edit".into(), path.display().to_string()], "\u{1b}:wq\r");
            // `None` on either side is an editor this harness could
            // not drive, not a difference between the two -- retried
            // like any other, and reported in its own words if it keeps
            // happening, so it can never be read as a divergence.
            if want != got || want.is_none() {
                let mut confirmed = None;
                for _ in 0..2 {
                    let want_again = run(
                        vec![VIM.into(), "-u".into(), "NONE".into(), "-i".into(), "NONE".into(), "-N".into(), path.display().to_string()],
                        ":wq\r",
                    );
                    let got_again = run(vec![bish.display().to_string(), "tool".into(), "edit".into(), path.display().to_string()], "\u{1b}:wq\r");
                    if want_again == got_again && want_again.is_some() {
                        confirmed = None;
                        break;
                    }
                    confirmed = Some((want_again, got_again));
                }
                if let Some((want, got)) = confirmed {
                    let unreachable = "<the editor never answered the handshake -- this harness could not drive it>";
                    differing.push((c.name, want.unwrap_or_else(|| unreachable.to_string()), got.unwrap_or_else(|| unreachable.to_string())));
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        let _ = std::fs::remove_dir(&root);
        differing
    }

    #[test]
    fn the_editor_agrees_with_vim() {
        let Some(bish) = bish_binary() else { return };
        if !have_vim() {
            return;
        }
        let differing = compare(CASES, &bish);
        assert!(
            differing.is_empty(),
            "{} of {} editor cases differ from vim:\n{}",
            differing.len(),
            CASES.len(),
            differing.iter().map(|(name, want, got)| format!("  {name}\n    vim : {want:?}\n    bish: {got:?}")).collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn the_known_editor_divergences_are_still_divergences() {
        let Some(bish) = bish_binary() else { return };
        if !have_vim() {
            return;
        }
        let differing: Vec<&str> = compare(PENDING, &bish).into_iter().map(|(name, _, _)| name).collect();
        for (name, why) in DIVERGENCES {
            assert!(differing.contains(name), "`{name}` matches vim now -- remove its line from DIVERGENCES ({why})");
        }
    }

    #[test]
    fn every_divergence_has_a_case_and_every_case_has_a_name() {
        for (name, _) in DIVERGENCES {
            assert!(PENDING.iter().any(|c| c.name == *name), "`{name}` is listed as a divergence with no case to prove it");
        }
        let mut names: Vec<&str> = CASES.iter().chain(PENDING).map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two cases share a name, so one of them cannot be reported");
    }
}
