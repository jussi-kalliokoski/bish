// How much of this thread's stack is left, so the shell can refuse a
// recursion instead of dying on one.
//
// Every recursive construct a script can write -- a function calling
// itself, `eval` calling itself, a file sourcing itself, a deeply
// parenthesised arithmetic expression -- runs on the Rust call stack,
// and running out of it is not an error that can be caught: the process
// aborts with `fatal runtime error: stack overflow` and dumps core.
// Real bash aborts on three of those four as well, which is not a
// standard worth matching.
//
// A depth counter per construct cannot answer this. Frames differ in
// size by an order of magnitude between constructs, the constructs
// nest inside each other, and the amount of stack available is not a
// constant either -- it is `ulimit -s`, which a user can and does
// change. What is actually running out is measured directly here
// instead: the distance between a local variable now and one recorded
// at startup, against the limit the kernel was asked for.

use std::cell::Cell;

// Standard Linux/glibc RLIMIT_STACK -- hardcoded for the same reason
// builtins/limits.rs hardcodes the rest of the RLIMIT_* numbers: it is
// a stable ABI constant and libc is already linked.
const RLIMIT_STACK: i32 = 3;

const RLIM_INFINITY: u64 = u64::MAX;

// What to assume when the kernel says "unlimited" or will not say. 8MiB
// is Linux's own default and what every shell here is actually running
// with; an unlimited stack still grows into something eventually, and a
// budget derived from a real number beats one derived from `u64::MAX`.
const ASSUMED_STACK: u64 = 8 * 1024 * 1024;

// The fraction of the stack a script's own nesting may use up. The rest
// is not slack: unwinding out of a refused recursion runs `Drop` for
// every frame on the way, the error path formats and prints a message,
// and an interactive shell then goes back to a prompt and redraws --
// all of it on the stack that is left. Leaving a third of the stack for
// that is not generous, it is the difference between reporting the
// problem and dying while reporting it.
const BUDGET_NUMERATOR: u64 = 2;
const BUDGET_DENOMINATOR: u64 = 3;

// The same idea for the arithmetic parser, which gets to keep going
// after everything else has given up.
//
// Its frames are tiny next to an evaluator frame, so it is almost never
// what filled the stack -- but it is often what is running when the
// stack finally runs out, since a function body tends to end up
// evaluating something. Sharing one threshold meant a runaway *function*
// was reported as `((d+1)): expression nested too deeply`, naming the
// innermost construct instead of the one at fault. Letting the parser
// run further keeps the diagnostic on whoever actually recursed.
const DEEP_NUMERATOR: u64 = 5;
const DEEP_DENOMINATOR: u64 = 6;

thread_local! {
    // Address of a local at the outermost point this thread knows
    // about. Zero until `note_base` runs; see `used` for what that
    // means.
    static BASE: Cell<usize> = const { Cell::new(0) };
    // `budget()` memoized -- a `getrlimit` per function call would be a
    // syscall on the hottest path in the shell.
    static BUDGET: Cell<usize> = const { Cell::new(0) };
}

/// Records this point as the bottom of the stack budget. Called once,
/// as early in `main` as there is a `main` to call it from.
///
/// Not calling it is safe: `used` falls back to the first address it is
/// ever asked about, which is deeper than the real base and so yields a
/// smaller number than the truth. That errs toward letting a recursion
/// continue, which is why the fallback is not enough on its own and
/// `main` calls this -- but it does keep a `Shell` built directly by a
/// test from measuring against zero.
pub fn note_base() {
    BASE.with(|b| b.set(here()));
}

/// The address of a local in *this* frame.
///
/// `black_box` and an explicit binding rather than `&0u8`: a reference
/// to a literal is const-promoted to static memory, so the obvious
/// spelling of this measures the address of a constant in the binary
/// and reports that the stack never moves at all.
#[inline(never)]
fn here() -> usize {
    let anchor = 0u8;
    let addr = &anchor as *const u8 as usize;
    std::hint::black_box(&anchor);
    addr
}

/// How many bytes of stack have been used since `note_base`.
fn used() -> usize {
    let here = here();
    BASE.with(|b| {
        if b.get() == 0 {
            b.set(here);
        }
        // Stacks grow downward on every platform this runs on, so the
        // base is the higher address. `saturating_sub` rather than a
        // subtraction: a thread whose stack sits above the one that
        // called `note_base` would otherwise underflow to a huge
        // number and refuse everything.
        b.get().saturating_sub(here)
    })
}

/// How many bytes of stack a script's nesting may use.
fn budget() -> usize {
    let cached = BUDGET.with(|b| b.get());
    if cached != 0 {
        return cached;
    }
    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }
    unsafe extern "C" {
        fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
    }
    let mut lim = RLimit { cur: 0, max: 0 };
    let total = match unsafe { getrlimit(RLIMIT_STACK, &mut lim) } {
        0 if lim.cur != RLIM_INFINITY && lim.cur != 0 => lim.cur,
        _ => ASSUMED_STACK,
    };
    let value = (total / BUDGET_DENOMINATOR * BUDGET_NUMERATOR).min(usize::MAX as u64) as usize;
    BUDGET.with(|b| b.set(value));
    value
}

/// Whether the stack is too far gone to recurse again.
///
/// Asked *before* descending, not after, so the answer still has the
/// whole remaining third of the stack to be acted on in.
pub fn nearly_exhausted() -> bool {
    used() >= budget()
}

/// `nearly_exhausted` for the arithmetic parser, which is allowed
/// further down than anything else -- see `DEEP_NUMERATOR`.
pub fn deeply_exhausted() -> bool {
    let total = budget() / BUDGET_NUMERATOR as usize * BUDGET_DENOMINATOR as usize;
    used() >= total / DEEP_DENOMINATOR as usize * DEEP_NUMERATOR as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_is_a_fraction_of_the_real_stack_and_is_memoized() {
        let first = budget();
        assert!(first > 0, "a budget of zero would refuse the first call ever made");
        assert!(first < ASSUMED_STACK as usize * 16, "and one this large is not a stack limit at all");
        assert_eq!(budget(), first, "memoized, since this is consulted per function call");
    }

    #[test]
    fn a_fresh_stack_is_not_nearly_exhausted() {
        note_base();
        assert!(!nearly_exhausted());
        assert!(!deeply_exhausted());
        assert!(used() < budget());
    }

    // The order matters, not the numbers: whatever fills the stack, the
    // ordinary limit has to be reached first, so the construct that was
    // actually recursing is the one named in the message.
    #[test]
    fn the_parsers_limit_comes_after_everyone_elses() {
        let ordinary = budget();
        let deep = budget() / BUDGET_NUMERATOR as usize * BUDGET_DENOMINATOR as usize / DEEP_DENOMINATOR as usize * DEEP_NUMERATOR as usize;
        assert!(deep > ordinary, "the parser must be allowed further down than a function call: {deep} vs {ordinary}");
    }

    // The measurement, rather than the policy: recursing must move the
    // number, and it must move in the direction of "less left".
    #[test]
    fn used_grows_as_the_stack_does() {
        note_base();
        fn descend(n: usize, seen: &mut Vec<usize>) {
            // Something big enough per frame that the difference is not
            // lost to a tail call or an inlined nothing.
            let ballast = [0u8; 4096];
            seen.push(used());
            if n > 0 {
                descend(n - 1, seen);
            }
            std::hint::black_box(&ballast);
        }
        let mut seen = Vec::new();
        descend(16, &mut seen);
        assert!(seen.windows(2).all(|w| w[1] > w[0]), "each frame has used more than the one above it: {seen:?}");
    }
}
