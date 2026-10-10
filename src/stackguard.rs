// How much of this thread's stack is left, so the shell can refuse a
// recursion instead of dying of one.
//
// Every recursive construct a script can write (a function calling
// itself, `eval`, a file sourcing itself, deeply nested arithmetic) runs
// on the Rust stack, and running out aborts the process with no chance
// to recover.
//
// A depth counter per construct can't answer this: frame sizes differ
// by an order of magnitude, constructs nest in each other, and the
// stack size is `ulimit -s`, which users change. So the stack used is
// measured directly: the distance from a local recorded at startup to
// one now, against the limit the kernel was asked for.

use std::cell::Cell;

// What to assume when the kernel says "unlimited" or won't say: 8 MiB,
// Linux's default. An unlimited stack still ends somewhere, and a real
// number makes a better budget than `u64::MAX`.
const ASSUMED_STACK: u64 = 8 * 1024 * 1024;

// The fraction of the stack a script's nesting may use. The rest isn't
// slack: unwinding a refused recursion runs every frame's `Drop`, the
// error is formatted and printed, and an interactive shell redraws its
// prompt, all on what's left.
const BUDGET_NUMERATOR: u64 = 2;
const BUDGET_DENOMINATOR: u64 = 3;

// The same for the arithmetic parser, which may go further.
//
// Its frames are tiny, so it rarely filled the stack, but it's often
// what's running when it fills, as function bodies evaluate things. A
// shared threshold reported a runaway function as `((d+1)): expression
// nested too deeply`; a deeper limit keeps the message on whatever
// recursed.
const DEEP_NUMERATOR: u64 = 5;
const DEEP_DENOMINATOR: u64 = 6;

thread_local! {
    // Address of a local at the outermost point this thread knows. Zero
    // until `note_base` runs; see `used`.
    static BASE: Cell<usize> = const { Cell::new(0) };
    // `budget()` memoised: a `getrlimit` per function call would be a
    // syscall on the hottest path.
    static BUDGET: Cell<usize> = const { Cell::new(0) };
}

/// Records this point as the bottom of the stack budget. Called once,
/// early in `main`.
///
/// Not calling it is safe: `used` falls back to the first address it's
/// asked about, which is above the real base and so undercounts, erring
/// toward allowing recursion. It keeps a `Shell` built directly by a test
/// from measuring against zero.
pub fn note_base() {
    BASE.with(|b| b.set(here()));
}

/// The address of a local in this frame. `black_box` and a binding, not
/// `&0u8`: a reference to a literal is promoted to static memory, which
/// never moves.
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
        // Stacks grow downward everywhere this runs, so the base is higher.
        // Saturating: a thread whose stack sits above the base's would
        // otherwise underflow and refuse everything.
        b.get().saturating_sub(here)
    })
}

/// How many bytes of stack a script's nesting may use.
fn budget() -> usize {
    let cached = BUDGET.with(|b| b.get());
    if cached != 0 {
        return cached;
    }
    let total = crate::platform::stack_limit().unwrap_or(ASSUMED_STACK);
    let value = (total / BUDGET_DENOMINATOR * BUDGET_NUMERATOR).min(usize::MAX as u64) as usize;
    BUDGET.with(|b| b.set(value));
    value
}

/// Points the guard at a different stack, returning the one it used. A
/// coroutine runs on its own mapping, so measuring against the thread's
/// base gives the distance between two unrelated mappings, and its
/// budget is a fixed allocation rather than `ulimit -s`.
/// `Coroutine::resume` swaps both in and out. Unused on targets without
/// stack switching.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub fn swap_stack(base: usize, budget_bytes: usize) -> (usize, usize) {
    let previous_base = BASE.with(|b| b.replace(base));
    let previous_budget = BUDGET.with(|b| b.replace(budget_bytes));
    (previous_base, previous_budget)
}

/// Whether the stack is too far gone to recurse again. Asked before
/// descending, so the remaining third is there to act on the answer.
pub fn nearly_exhausted() -> bool {
    used() >= budget()
}

/// `nearly_exhausted` for the arithmetic parser, allowed further down;
/// see `DEEP_NUMERATOR`.
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

    // Whatever fills the stack, the ordinary limit is reached first, so the
    // construct actually recursing is named.
    #[test]
    fn the_parsers_limit_comes_after_everyone_elses() {
        let ordinary = budget();
        let deep = budget() / BUDGET_NUMERATOR as usize * BUDGET_DENOMINATOR as usize / DEEP_DENOMINATOR as usize * DEEP_NUMERATOR as usize;
        assert!(deep > ordinary, "the parser must be allowed further down than a function call: {deep} vs {ordinary}");
    }

    // Recursing moves the number toward less left.
    #[test]
    fn used_grows_as_the_stack_does() {
        note_base();
        fn descend(n: usize, seen: &mut Vec<usize>) {
            // Big enough per frame not to be lost to a tail call or inlining.
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
