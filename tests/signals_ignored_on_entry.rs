//! A signal ignored when bish starts can't be trapped or reset.
//!
//! POSIX's rule, and bash's behaviour, so immunity handed down survives: a
//! script backgrounded by another has INT and QUIT ignored so a Ctrl-C at
//! the terminal doesn't kill it, and `nohup` relies on the same for HUP.
//!
//! Not in `bashdiff.rs`: this is about the state bish is entered in, which
//! only its parent can set up, in `pre_exec` between fork and exec.

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIG_IGN: usize = 1;
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// A bish running `script`, optionally entered with `sig` ignored, which
/// only a `pre_exec` can arrange.
fn spawn(script: &str, ignore: Option<i32>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bish"));
    command.arg("-c").arg(script).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(sig) = ignore {
        unsafe {
            command.pre_exec(move || {
                signal(sig, SIG_IGN);
                Ok(())
            });
        }
    }
    command.spawn().expect("bish could not be run")
}

/// Waits up to `limit` for it to exit; `None` if it's still running.
fn wait_for(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Long enough for the shell to reach its loop.
fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

const LOOPS_FOREVER: &str = "trap 'echo TRAPPED; exit 7' SIGNAL\nwhile :; do i=$((i+1)); done";

fn script_for(name: &str) -> String {
    LOOPS_FOREVER.replace("SIGNAL", name)
}

/// The control: without it, a `trap` that never worked would pass as
/// well.
#[test]
fn a_signal_left_alone_on_entry_is_trapped_normally() {
    for (name, sig) in [("TERM", SIGTERM), ("INT", SIGINT), ("HUP", SIGHUP)] {
        let mut child = spawn(&script_for(name), None);
        settle();
        assert_eq!(unsafe { kill(child.id() as i32, sig) }, 0, "{name}: could not signal");
        let status = match wait_for(&mut child, Duration::from_secs(5)) {
            Some(status) => status,
            None => {
                let _ = child.kill();
                panic!("{name}: the trap never fired -- still running");
            }
        };
        assert_eq!(status.code(), Some(7), "{name}: the trap's own `exit 7`");
    }
}

/// The rule itself.
#[test]
fn a_signal_ignored_on_entry_cannot_be_trapped() {
    for (name, sig) in [("TERM", SIGTERM), ("INT", SIGINT), ("HUP", SIGHUP)] {
        let mut child = spawn(&script_for(name), Some(sig));
        settle();
        assert_eq!(unsafe { kill(child.id() as i32, sig) }, 0, "{name}: could not signal");
        // Short, as this asserts nothing happens, and a wrongly effective
        // `trap` fires at once.
        if let Some(status) = wait_for(&mut child, Duration::from_millis(600)) {
            panic!("{name}: the trap took effect on a signal ignored before bish started ({status:?})");
        }
        // It can't die of the signal either, so it's killed.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// `trap` still succeeds, with no effect, as in bash: a script trapping
/// INT defensively shouldn't fail because its parent was careful.
#[test]
fn trapping_an_ignored_signal_is_not_an_error() {
    let child = spawn("trap 'echo X' TERM; echo status=$?", Some(SIGTERM));
    let out = child.wait_with_output().expect("output");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "status=0\n");
}
