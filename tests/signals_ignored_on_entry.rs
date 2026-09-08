//! A signal ignored when bish starts cannot be trapped or reset.
//!
//! POSIX's rule, and bash's behaviour. It exists so that immunity handed
//! down from outside survives: a script backgrounded from another script
//! has INT and QUIT ignored precisely so a Ctrl-C aimed at the terminal's
//! process group does not take it down along with everything else, and
//! `nohup`'s promise about HUP works the same way. A shell whose `trap`
//! could override that would hand back the very thing the caller went out
//! of its way to take away.
//!
//! **Not in `bashdiff.rs`, and it cannot be.** That corpus is one script
//! string run through two shells, which is the right shape for almost
//! everything -- but this is not about what a script says, it is about
//! the state the shell was *entered* in. Only the thing that spawns the
//! shell can set that up, so the test has to be out here where it can be
//! the parent: `pre_exec` runs between fork and exec, which is the one
//! moment where the disposition can be made to arrive already ignored.

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

/// A bish running `script`, optionally entered with `sig` already
/// ignored -- which is the whole point, and why this is a `pre_exec`
/// rather than anything the script itself could arrange.
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

/// Waits up to `limit` for it to exit. `None` means it outlived that,
/// and the caller has to deal with it.
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

/// Long enough for the shell to have reached its loop and be running it.
fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

const LOOPS_FOREVER: &str = "trap 'echo TRAPPED; exit 7' SIGNAL\nwhile :; do i=$((i+1)); done";

fn script_for(name: &str) -> String {
    LOOPS_FOREVER.replace("SIGNAL", name)
}

/// The control, and it matters as much as the case below: without it,
/// a `trap` that never worked at all would pass just as well.
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

/// And the rule itself.
#[test]
fn a_signal_ignored_on_entry_cannot_be_trapped() {
    for (name, sig) in [("TERM", SIGTERM), ("INT", SIGINT), ("HUP", SIGHUP)] {
        let mut child = spawn(&script_for(name), Some(sig));
        settle();
        assert_eq!(unsafe { kill(child.id() as i32, sig) }, 0, "{name}: could not signal");
        // Deliberately short: this is asserting that *nothing* happens,
        // and a test which asserts nothing happens should not spend long
        // proving it. A `trap` that wrongly took effect fires at once.
        if let Some(status) = wait_for(&mut child, Duration::from_millis(600)) {
            panic!("{name}: the trap took effect on a signal ignored before bish started ({status:?})");
        }
        // It cannot die of the signal either, so it needs taking down.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// `trap` still *succeeds* -- it is accepted and simply has no effect.
/// bash reports success here, and a script writing `trap ... INT`
/// defensively should not start failing because somebody upstream was
/// careful on its behalf.
#[test]
fn trapping_an_ignored_signal_is_not_an_error() {
    let child = spawn("trap 'echo X' TERM; echo status=$?", Some(SIGTERM));
    let out = child.wait_with_output().expect("output");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "status=0\n");
}
