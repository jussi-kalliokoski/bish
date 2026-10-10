// A corpus of editor keystrokes vim and bish's editor should agree on,
// run through both and diffed, as `bashdiff.rs` does for the shell. A
// case is one line.
//
// The file is compared, never the screen: both editors get the same
// content and keys, then `:wq`, and the bytes on disk are diffed, so
// nothing depends on rendering or cursor placement.
//
// - Keys go in one at a time: `dt,` arriving in one read differs from
//   someone typing it.
// - vim runs as `vim -u NONE -i NONE -N` (no vimrc, no viminfo,
//   nocompatible), or the corpus measures the local configuration.
//
// Skipped where there's no vim.

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct Case {
        name: &'static str,
        /// The file's bytes before the keys. Bytes, as what an editor does
        /// with non-UTF-8 is part of what's measured (see `case_raw`).
        before: &'static [u8],
        /// Sent one character at a time, in order.
        keys: &'static str,
    }

    const fn case(name: &'static str, before: &'static str, keys: &'static str) -> Case {
        Case { name, before: before.as_bytes(), keys }
    }

    /// A case whose file can't be written as Rust text: a latin-1 byte, a
    /// byte-order mark.
    const fn case_raw(name: &'static str, before: &'static [u8], keys: &'static str) -> Case {
        Case { name, before, keys }
    }

    // `\u{1b}` is Escape and `\r` Enter; both are ordinary keys here.
    const CASES: &[Case] = &[
        // -- deleting --------------------------------------------------
        case("dd", "one\ntwo\nthree\n", "dd"),
        case("cc", "abc\ndef\n", "ccZ\u{1b}"),
        case("cc-with-count", "a\nb\nc\n", "2ccZ\u{1b}"),
        case("r", "abc\n", "rZ"),
        case("r-with-count", "abcdef\n", "3rZ"),
        // `Z` is `ZZ`'s first key, intercepted before the key machine, so
        // `rZ` catches an interception that ignores what's pending.
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
        // A count before an insert repeats what's typed; before `.` it
        // replaces the change's count.
        case("insert-with-count", "a\n", "3ihi\u{1b}"),
        case("append-at-end-with-count", "a\n", "2Abc\u{1b}"),
        case("open-line-with-count", "a\nb\n", "3oX\u{1b}"),
        case("open-line-above-with-count", "a\nb\n", "j2OX\u{1b}"),
        case("replace-mode-with-count", "abcdef\n", "2Rxy\u{1b}"),
        case("dot-repeats-a-counted-insert", "a\nb\n", "2iX\u{1b}j."),
        case("dot-with-a-count-replaces-the-count", "1\n2\n3\n4\n5\n6\n", "3ddj2."),
        case("dot-with-nothing-to-repeat", "abc\n", "."),
        case("mark-linewise-delete", "a\nb\nc\n", "maGd'a"),
        case("mark-linewise-delete-partial", "a\nb\nc\nd\n", "jmaGd'a"),
        // An empty file and a file of one empty line differ, and a buffer
        // that always has a line for the cursor must remember which it was.
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
        // -- a sweep of less-trodden ground --------------------------------
        // Joining, and the one-key shorthand operators.
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
        // More ex forms.
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
        // The editor's `/` is ERE with GNU's zero-width assertions; `\<`,
        // `\>`, `\w` and `\s` mean the same in vim.
        case("search-word-start", "foobar\nfoo bar\n", "/\\<bar\rx"),
        case("search-word-end", "barn\nfoo bar\n", "/bar\\>\rx"),
        case("search-word-class", "  %%\nab12\n", "/\\w\\w\rx"),
        case("search-space-class", "ab\na b\n", "/\\s\rx"),
        // -- encodings ------------------------------------------------- A
        // file that isn't UTF-8 is still a file: editing one line must
        // leave the other bytes alone.
        case_raw("latin1-file-keeps-its-bytes", b"caf\xe9\nna\xefve\n", "jx"),
        case_raw("latin1-file-edited-on-the-accented-line", b"caf\xe9\n", "$x"),
        // A byte-order mark isn't a character, and isn't the editor's to
        // drop: whatever reads the file next may look for it.
        case_raw("utf8-bom-survives-an-edit", b"\xef\xbb\xbfone\ntwo\n", "jx"),
        case_raw("utf16-file-keeps-its-encoding", b"\xff\xfeo\x00n\x00e\x00\n\x00t\x00w\x00o\x00\n\x00", "jx"),
        // -- folds -----------------------------------------------------
        // Only the file is compared, so a fold shows by what an edit does:
        // a closed fold is one line to `j`, `k`, `dd` and counts, and all
        // its lines to an operator. `vim -u NONE` has manual folding and
        // `foldlevel` 0, so a new fold starts closed there too.
        case("fold-dd-deletes-a-closed-fold", "1\n2\n3\n4\n", "zfjdd"),
        case("fold-j-steps-over-a-closed-fold", "1\n2\n3\n4\n", "zfjjdd"),
        case("fold-k-steps-over-a-closed-fold", "1\n2\n3\n4\n", "jzfjGkkdd"),
        case("fold-a-count-counts-a-fold-as-one-line", "1\n2\n3\n4\n5\n", "zfj2dd"),
        case("fold-dj-takes-the-fold-and-the-line-after", "1\n2\n3\n4\n", "zfjdj"),
        case("fold-dl-deletes-the-whole-fold", "ab\ncd\nef\n", "zfjdl"),
        case("fold-yy-yanks-every-line-of-it", "1\n2\n3\n", "zfjyyGp"),
        case("fold-zo-opens-it", "1\n2\n3\n4\n", "zfjzojdd"),
        case("fold-zc-closes-it-again-from-inside", "1\n2\n3\n4\n", "zfjzojzcdd"),
        case("fold-za-toggles-it", "1\n2\n3\n4\n", "zfjzajzadd"),
        case("fold-zR-opens-everything", "1\n2\n3\n4\n", "zfjzRjdd"),
        case("fold-zE-removes-every-fold", "1\n2\n3\n4\n", "zfjzEjdd"),
        case("fold-zd-removes-the-fold", "1\n2\n3\n4\n", "zfjzdjdd"),
        case("fold-zF-folds-count-lines", "1\n2\n3\n4\n", "3zFjdd"),
        case("fold-visual-zf-folds-the-selection", "1\n2\n3\n4\n", "Vjzfjdd"),
        case("fold-ex-fold-folds-a-range", "1\n2\n3\n4\n", ":2,3fold\rggjdd"),
        case("fold-zj-moves-to-the-next-fold", "1\n2\n3\n4\n5\n", "jjzfjggzjdd"),
        case("fold-nested-zo-opens-one-level", "1\n2\n3\n4\n", "zfjzfjzodd"),
        // What moves by rows stops on a closed fold; a search opens the fold
        // it lands in.
        case("fold-search-opens-the-fold-it-lands-in", "one\ntwo\nthree\nfour\n", "jjzfjgg/four\rdd"),
        case("fold-l-opens-the-fold", "ab\ncd\nef\n", "zfjlx"),
        // A fold moves with its lines.
        case("fold-moves-with-a-line-opened-above", "1\n2\n3\n4\n", "jzfjggOnew\u{1b}jjdd"),
        case("fold-survives-an-undo", "1\n2\n3\n4\n", "zfjGdduggdd"),
    ];

    // Cases bish doesn't match yet, each with why. Asserted to still differ,
    // so a fixed one fails until its entry goes, as in `bashdiff.rs`.
    const DIVERGENCES: &[(&str, &str)] = &[
        (
            "indent-width",
            "`>>` inserts four spaces where `vim -u NONE` inserts a tab -- bish reads .editorconfig and defaults to spaces, which is a choice rather than a bug, but it is a difference and it is recorded",
        ),
        ("visual-indent", "indents with spaces where `vim -u NONE` uses a tab -- the same deliberate choice as `indent-width`"),
        // Both shift the right lines, with spaces where vim uses a tab.
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

    /// The vim this machine would run, or `None`.
    fn vim() -> Option<String> {
        crate::toolpath::find("vim")
    }

    /// `target/<profile>/bish`, from the test binary's path, as in
    /// `bashdiff.rs`.
    fn bish_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.parent()?.join("bish");
        path.exists().then_some(path)
    }

    /// A file's bytes for a failure message: UTF-8 prints as itself, anything
    /// else as escaped bytes behind a marker, so the two never look alike.
    fn render(bytes: Vec<u8>) -> String {
        match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => {
                let escaped: String = e.into_bytes().iter().map(|b| format!("\\x{b:02x}")).collect();
                format!("<bytes>{escaped}")
            }
        }
    }

    /// Runs one editor in a pty over `path`, sends `keys` one character at a
    /// time and then `save`, and returns what the file holds. `Err` when the
    /// editor never answered the handshake, a harness failure rather than a
    /// difference (see the call site).
    fn edit(argv: &[String], path: &Path, keys: &str, save: &str) -> Result<String, &'static str> {
        let Ok(pty) = crate::pty::open() else { return Err("no pty could be opened for it") };
        let _ = crate::pty::set_size(std::os::fd::AsRawFd::as_raw_fd(&pty.master), 24, 80);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.env("TERM", "xterm-256color");
        // No config for either editor: this measures editors, not the
        // machine.
        cmd.env_remove("VIMINIT");
        // Nor the clipboard. bish's unnamed register is the system
        // clipboard, and each `wl-copy` daemonizes holding the stdout it
        // inherited, so a corpus full of `dd` would fill the clipboard and
        // keep a piped `cargo test` from ever reaching EOF.
        cmd.env("BISH_NO_CLIPBOARD", "1");
        let Ok(mut child) = crate::pty::spawn_attached(cmd, &pty.slave_path) else {
            return Err("could not be started");
        };
        let mut master = pty.master;
        let _ = crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&master));
        // Every read goes through here, and `settle` judges quiet by what
        // this returns; reading elsewhere too let keys go out while the
        // editor was still painting.
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
        // Wait for the first screen before typing: an editor not yet
        // started is quiet too, and a key sent before raw mode goes to the
        // line discipline. An editor that never answers is a harness
        // failure, not a divergence: typing anyway leaves the file
        // unchanged, which looks like "ignored every key".
        //
        // Thirty seconds: returning on the answer, a healthy run never
        // waits, and a run beside the whole suite needs the room.
        if !handshake(&mut master, &mut drain, Duration::from_secs(30)) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("never answered the handshake");
        }
        for c in keys.chars().chain(save.chars()) {
            let mut buf = [0u8; 4];
            let _ = master.write_all(c.encode_utf8(&mut buf).as_bytes());
            let floor = if c == '\u{1b}' { ESC_DWELL } else { Duration::from_millis(20) };
            settle(&mut master, &mut drain, floor, KEY_LIMIT, QUIET_TICKS);
        }
        // Wait for it to finish writing, killing it if it won't, and say
        // so: a `:wq` that hasn't finished leaves the input on disk, which
        // reads as "ignored every key". An `Err`, like a missed handshake.
        let mut exited = false;
        let deadline = Instant::now() + EXIT_LIMIT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
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
        match exited {
            // Compared as bytes: read as UTF-8, every latin-1 file would
            // become the same error message on both sides.
            true => Ok(std::fs::read(path).map(render).unwrap_or_else(|e| format!("<unreadable: {e}>"))),
            // Distinct from the handshake failure: different causes,
            // different fixes.
            false => Err("never finished writing the file"),
        }
    }

    /// Blocks until the editor has demonstrably read a key, by asking it
    /// something it must answer on screen: Ctrl-L redraws in both editors
    /// and changes nothing.
    ///
    /// Waiting for output can't establish this: an editor writes several
    /// times on its way up and is quiet between, so "gone quiet" and "not
    /// started" look alike, and a key sent before raw mode is swallowed by
    /// the line discipline. Asking until answered encodes no guess about
    /// machine speed. The wait for quiet before each ask matters: otherwise
    /// the startup paint counts as the answer.
    ///
    /// Ctrl-L rather than `:`, which needs cancelling with Esc, and an Esc
    /// just before a case's first key can merge with it (`Esc O` is SS3).
    ///
    /// Returns whether the editor answered. Typing anyway would leave the
    /// file untouched and report a difference that's the harness's.
    fn handshake(master: &mut std::fs::File, drain: &mut impl FnMut(&mut std::fs::File) -> usize, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        // First wait for the editor to take the terminal: the pty's ends
        // share line settings, so ICANON going off is the editor entering
        // raw mode. An answer alone can be faked by a startup paint with a
        // gap in it.
        let fd = std::os::fd::AsRawFd::as_raw_fd(&*master);
        while Instant::now() < deadline && !crate::term::is_raw(fd) {
            drain(master);
            std::thread::sleep(Duration::from_millis(2));
        }
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

    /// How long to leave an Escape before the next key. bish takes Esc as
    /// alone if nothing follows within `ESCAPE_TIMEOUT_MS` (30ms, editor.rs);
    /// a byte arriving sooner that isn't `[` or `O` decodes the pair as
    /// `Key::Unknown`, losing both, so `Esc :wq` at pty speed did nothing.
    const ESC_DWELL: Duration = Duration::from_millis(90);

    /// Consecutive silent 4ms polls meaning the editor finished responding.
    /// bish answers a key in several chunks with gaps, so a short silence
    /// can fall inside one response and the next key goes out mid-write,
    /// which `ESC_DWELL` exists to avoid.
    const QUIET_TICKS: u32 = 40;

    /// How long one keystroke may take before the next is sent anyway.
    /// Generous because it's what makes `ESC_DWELL` work: the dwell must
    /// separate bytes as the editor reads them, and bytes written while it's
    /// still redrawing just queue. A debug build on a loaded machine can take
    /// most of a second per key.
    const KEY_LIMIT: Duration = Duration::from_millis(2500);

    /// How long `:wq` may take before the editor is given up on: as long as
    /// the handshake's budget, since a debug build writing a file beside the
    /// suite is slow, and giving up reported the unedited file as bish's.
    const EXIT_LIMIT: Duration = Duration::from_secs(30);

    /// Reads until the editor is silent for `quiet_ticks` consecutive 4ms
    /// polls, or `limit` passes; adaptive, so fast keys don't pay for slow
    /// ones. `floor` matters too: a key that changes nothing on screen
    /// produces no output, so quiet alone can't tell done from not started.
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

    /// A case whose editors disagreed: its name, vim's file and bish's.
    type Divergence = (&'static str, String, String);

    /// A case this harness couldn't drive: its name, and which editor failed
    /// how.
    type Undriveable = (&'static str, String);

    /// The cases whose files differ, and separately those this harness
    /// couldn't drive. Not driving an editor is the harness losing a race on
    /// a busy machine, not a fact about either editor, so the two are kept
    /// apart.
    ///
    /// A differing case is rerun twice before it's believed: a key lost to a
    /// slow redraw looks like a real divergence, but a real one survives
    /// every rerun. Only differing cases pay for reruns.
    fn compare(cases: &[Case], bish: &Path, vim: &str) -> (Vec<Divergence>, Vec<Undriveable>) {
        let root = std::env::temp_dir().join(format!("bish-vimdiff-{}", std::process::id()));
        let mut differing = Vec::new();
        let mut undriveable = Vec::new();
        // `BISH_VIMDIFF_ONLY=name` runs one case; the whole corpus takes
        // minutes.
        let only = std::env::var("BISH_VIMDIFF_ONLY").ok();
        for c in cases {
            if only.as_deref().is_some_and(|want| want != c.name) {
                continue;
            }
            // A directory per case, named after it: two tests share this
            // root, and an index would collide.
            let dir = root.join(c.name);
            let path = dir.join("f.txt");
            let run = |argv: Vec<String>, save: &str| -> Result<String, &'static str> {
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, c.before).unwrap();
                edit(&argv, &path, c.keys, save)
            };
            let want =
                run(vec![vim.to_string(), "-u".into(), "NONE".into(), "-i".into(), "NONE".into(), "-N".into(), path.display().to_string()], ":wq\r");
            // bish gets an extra Escape first, or a case ending in insert
            // mode would type `:wq` into the buffer; every case leaves vim
            // in normal mode.
            let got = run(vec![bish.display().to_string(), "tool".into(), "edit".into(), path.display().to_string()], "\u{1b}:wq\r");
            // An `Err` on either side is a harness failure: retried like
            // any other, and reported apart from divergences.
            if want != got || want.is_err() {
                let mut confirmed = None;
                for _ in 0..2 {
                    let want_again = run(
                        vec![vim.to_string(), "-u".into(), "NONE".into(), "-i".into(), "NONE".into(), "-N".into(), path.display().to_string()],
                        ":wq\r",
                    );
                    let got_again = run(vec![bish.display().to_string(), "tool".into(), "edit".into(), path.display().to_string()], "\u{1b}:wq\r");
                    if want_again == got_again && want_again.is_ok() {
                        confirmed = None;
                        break;
                    }
                    confirmed = Some((want_again, got_again));
                }
                match confirmed {
                    // Couldn't be driven, three times running: the
                    // harness's failure, kept apart from divergences.
                    Some((Err(why), _)) => undriveable.push((c.name, format!("vim {why}"))),
                    Some((_, Err(why))) => undriveable.push((c.name, format!("bish {why}"))),
                    Some((Ok(want), Ok(got))) => differing.push((c.name, want, got)),
                    None => {}
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        let _ = std::fs::remove_dir(&root);
        (differing, undriveable)
    }

    /// How many cases may be undriveable before the run stops counting as
    /// evidence. Not zero: a machine running the rest of the suite loses one
    /// now and then. Not unbounded: an editor that stopped answering would
    /// pass. Under heavy load (load average 6 to 12) runs lost 5 and 6
    /// cases. An editor that stopped answering loses all of them, and every
    /// undriveable case is printed either way.
    const UNDRIVEABLE_TOLERANCE: usize = 10;

    #[test]
    fn the_editor_agrees_with_vim() {
        let Some(bish) = bish_binary() else { return };
        let Some(vim) = vim() else { return };
        let (differing, undriveable) = compare(CASES, &bish, &vim);
        assert!(
            differing.is_empty(),
            "{} of {} editor cases differ from vim:\n{}",
            differing.len(),
            CASES.len(),
            differing.iter().map(|(name, want, got)| format!("  {name}\n    vim : {want:?}\n    bish: {got:?}")).collect::<Vec<_>>().join("\n")
        );
        // Printed even within tolerance, so a run short of evidence can't
        // look clean.
        for (name, why) in &undriveable {
            eprintln!("vimdiff: {name}: {why} -- not counted either way");
        }
        assert!(
            undriveable.len() <= UNDRIVEABLE_TOLERANCE,
            "{} of {} cases could not be driven at all, which is too many to blame on a busy machine:\n{}",
            undriveable.len(),
            CASES.len(),
            undriveable.iter().map(|(name, why)| format!("  {name}: {why}")).collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn the_known_editor_divergences_are_still_divergences() {
        let Some(bish) = bish_binary() else { return };
        let Some(vim) = vim() else { return };
        // An undriveable case is no evidence a divergence was fixed: absent
        // from the differences alone, it would read as agreeing with vim.
        let (differing, undriveable) = compare(PENDING, &bish, &vim);
        let differing: Vec<&str> = differing.into_iter().map(|(name, _, _)| name).collect();
        for (name, why) in DIVERGENCES {
            if let Some((_, reason)) = undriveable.iter().find(|(n, _)| n == name) {
                eprintln!("vimdiff: {name}: {reason} -- no evidence either way, so its divergence stands");
                continue;
            }
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
