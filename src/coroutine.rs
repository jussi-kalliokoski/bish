// On wasm the stub never suspends, so `Suspended` and `Running` go unbuilt.
#![cfg_attr(target_family = "wasm", allow(dead_code))]

// Two interpreters running at once, in one process, on one thread.
//
// A pipeline's stages must run concurrently: a producer blocks when the
// pipe fills until the consumer drains it. A process per stage buys that,
// but a stage that needs a shell then costs a bish startup and knows only
// what a generated preamble tells it. bish's evaluator recurses on the
// native stack, so where an interpreter is *is* a stack pointer and the
// callee-saved registers, and switching between two means swapping those,
// onto a stack of our own.
//
// This module is that switch and nothing else; `scheduler` decides when
// to switch.
//
// # Why hand-written assembly
//
// libc's `swapcontext` makes a `sigprocmask` syscall on every switch to
// save the signal mask, which a stage never changes. This switch touches
// only registers.
//
// # Guarantees
//
// - A coroutine runs until it yields or returns; there's no preemption.
// - A panic never crosses a switch: the entry point catches it and reports
//   it to the resumer. Unwinding across a stack built here would be
//   undefined behaviour.
// - Each stack is its own mapping with an unreadable guard page below, so
//   an overrun faults instead of corrupting the next mapping.

#[cfg(not(target_family = "wasm"))]
use std::cell::Cell;

/// The callee-saved state of a suspended execution. Only the stack pointer
/// is stored: everything else is pushed onto that stack by the switch.
#[cfg(not(target_family = "wasm"))]
#[repr(C)]
#[derive(Default)]
struct Context {
    stack_pointer: *mut u8,
}

// Declares `bish_switch_context` as a global the binary can call but
// nothing outside can see. ELF and Mach-O spell that differently, and
// Mach-O prefixes C symbols with `_`.
#[cfg(not(target_family = "wasm"))]
#[cfg(target_vendor = "apple")]
macro_rules! switch_context_label {
    () => {
        ".globl _bish_switch_context\n.private_extern _bish_switch_context\n_bish_switch_context:"
    };
}

#[cfg(not(target_family = "wasm"))]
#[cfg(not(target_vendor = "apple"))]
macro_rules! switch_context_label {
    () => {
        ".globl bish_switch_context\n.hidden bish_switch_context\nbish_switch_context:"
    };
}

// x86_64 System V: rbx, rbp and r12-r15 are callee-saved, so they're
// pushed onto the outgoing stack and popped from the incoming one. The
// final `ret` does the jump: a stack from `Stack::prepare` has the
// trampoline where a return address would be.
#[cfg(target_arch = "x86_64")]
std::arch::global_asm!(
    switch_context_label!(),
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rdi], rsp",
    "mov rsp, [rsi]",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

// aarch64 AAPCS: x19-x28, the frame and link registers, and the low
// halves of v8-v15 are callee-saved. Same shape: push, swap stack
// pointers, pop, return through the link register.
#[cfg(target_arch = "aarch64")]
std::arch::global_asm!(
    switch_context_label!(),
    "sub sp, sp, #0xa0",
    "stp x19, x20, [sp, #0x00]",
    "stp x21, x22, [sp, #0x10]",
    "stp x23, x24, [sp, #0x20]",
    "stp x25, x26, [sp, #0x30]",
    "stp x27, x28, [sp, #0x40]",
    "stp x29, x30, [sp, #0x50]",
    "stp d8, d9, [sp, #0x60]",
    "stp d10, d11, [sp, #0x70]",
    "stp d12, d13, [sp, #0x80]",
    "stp d14, d15, [sp, #0x90]",
    "mov x2, sp",
    "str x2, [x0]",
    "ldr x2, [x1]",
    "mov sp, x2",
    "ldp x19, x20, [sp, #0x00]",
    "ldp x21, x22, [sp, #0x10]",
    "ldp x23, x24, [sp, #0x20]",
    "ldp x25, x26, [sp, #0x30]",
    "ldp x27, x28, [sp, #0x40]",
    "ldp x29, x30, [sp, #0x50]",
    "ldp d8, d9, [sp, #0x60]",
    "ldp d10, d11, [sp, #0x70]",
    "ldp d12, d13, [sp, #0x80]",
    "ldp d14, d15, [sp, #0x90]",
    "add sp, sp, #0xa0",
    "ret",
);

#[cfg(not(target_family = "wasm"))]
unsafe extern "C" {
    /// Saves the current execution into `from` and resumes `to`. Returns when
    /// something switches back into `from`: a call that can take arbitrarily
    /// long and run arbitrary code.
    fn bish_switch_context(from: *mut Context, to: *const Context);
}

/// Stack per coroutine. The evaluator uses kilobytes per nested construct
/// (see `stackguard`), so this allows a recursing stage. It's address
/// space, not memory: pages fault in as touched.
#[cfg(not(target_family = "wasm"))]
const STACK_SIZE: usize = 2 * 1024 * 1024;

/// An unreadable page below the stack, so an overrun faults there.
#[cfg(not(target_family = "wasm"))]
const GUARD_SIZE: usize = 4096;

/// One coroutine's stack: `mmap`ed, guarded, and unmapped on drop.
#[cfg(not(target_family = "wasm"))]
struct Stack {
    base: *mut u8,
    total: usize,
}

#[cfg(not(target_family = "wasm"))]
impl Stack {
    fn new() -> std::io::Result<Stack> {
        // The guard goes at the low end, which a downward-growing stack
        // overruns into.
        Ok(Stack { base: crate::platform::map_guarded_stack(GUARD_SIZE, STACK_SIZE)?, total: GUARD_SIZE + STACK_SIZE })
    }

    /// The highest usable address, aligned down to 16 bytes.
    fn top(&self) -> *mut u8 {
        let top = self.base as usize + self.total;
        (top & !0xf) as *mut u8
    }
}

#[cfg(not(target_family = "wasm"))]
impl Drop for Stack {
    fn drop(&mut self) {
        unsafe { crate::platform::unmap(self.base, self.total) };
    }
}

/// What a coroutine is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Never resumed yet.
    Fresh,
    /// Resumed at least once, now suspended in `yield_now`.
    Suspended,
    /// Running: the one this thread is inside.
    Running,
    /// Returned or panicked. Resuming again does nothing.
    Done,
}

// The wasm version. WASI has no second stack to switch to, so a
// coroutine can't suspend: `resume` runs the body through, `yield_now`
// does nothing, and `in_coroutine` is false.

#[cfg(target_family = "wasm")]
pub struct Coroutine {
    state: State,
    body: Option<Box<dyn FnOnce()>>,
    panicked: bool,
}

#[cfg(target_family = "wasm")]
impl Coroutine {
    pub fn new(body: impl FnOnce() + 'static) -> std::io::Result<Box<Coroutine>> {
        Ok(Box::new(Coroutine { state: State::Fresh, body: Some(Box::new(body)), panicked: false }))
    }

    pub fn resume(&mut self) -> State {
        if let Some(body) = self.body.take() {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            self.panicked = outcome.is_err();
        }
        self.state = State::Done;
        self.state
    }

    pub fn panicked(&self) -> bool {
        self.panicked
    }

    pub fn state(&self) -> State {
        self.state
    }
}

/// Does nothing: a wasm coroutine can't suspend.
#[cfg(target_family = "wasm")]
pub fn yield_now() {}

/// Always false on this platform.
#[cfg(target_family = "wasm")]
pub fn in_coroutine() -> bool {
    false
}

#[cfg(not(target_family = "wasm"))]
thread_local! {
    /// Where to switch back to when the running coroutine yields, set by
    /// `resume` before entering, so a `yield_now` at any depth knows.
    static RETURN_TO: Cell<*mut Context> = const { Cell::new(std::ptr::null_mut()) };
    /// The coroutine this thread is inside, for `yield_now` and the
    /// trampoline; null when none. The whole `Coroutine`, since deriving it
    /// from its `Context` would assume a field offset Rust doesn't promise.
    static CURRENT: Cell<*mut Coroutine> = const { Cell::new(std::ptr::null_mut()) };
}

/// A separately-stacked execution that can be stopped and restarted.
#[cfg(not(target_family = "wasm"))]
pub struct Coroutine {
    context: Context,
    /// The top of this coroutine's stack and how much a script may use,
    /// swapped into `stackguard` during each resume, so recursion in a stage
    /// is measured against the stack it runs on.
    stack_top: usize,
    stack_budget: usize,
    // Dropped last, once the coroutine can't be resumed; see `Drop`.
    stack: Option<Stack>,
    state: State,
    // The body, until the entry point takes it.
    body: Option<Box<dyn FnOnce()>>,
    // Set if the body panicked, so `resume` can report it.
    panicked: bool,
}

#[cfg(not(target_family = "wasm"))]
impl Coroutine {
    /// A coroutine that runs `body` when first resumed. Allocates the stack
    /// and arranges for the first `resume` to land at the top of `body`.
    pub fn new(body: impl FnOnce() + 'static) -> std::io::Result<Box<Coroutine>> {
        let stack = Stack::new()?;
        let stack_top = stack.top() as usize;
        let mut co = Box::new(Coroutine {
            context: Context::default(),
            stack_top,
            // The same two thirds `stackguard` leaves on the thread's
            // stack, for unwinding and reporting.
            stack_budget: STACK_SIZE / 3 * 2,
            stack: Some(stack),
            state: State::Fresh,
            body: Some(Box::new(body)),
            panicked: false,
        });
        co.prepare();
        Ok(co)
    }

    /// Lays out the initial stack so the first switch lands in
    /// `trampoline`. It mirrors what `bish_switch_context` pops: six
    /// registers, then a return address, which `ret` takes, entering the
    /// trampoline without a call.
    #[cfg(target_arch = "x86_64")]
    fn prepare(&mut self) {
        let top = self.stack.as_ref().expect("a fresh coroutine has its stack").top();
        // System V wants rsp+8 to be 16-aligned at a function's first
        // instruction, as right after a `call`. `ret` pops the address
        // below, so that slot plays the pushed return address.
        let self_ptr = self as *mut Coroutine as usize;
        unsafe {
            let mut sp = top as *mut usize;
            // A zero return address under the trampoline: returning from it
            // instead of switching faults at once.
            sp = sp.sub(1);
            sp.write(0);
            sp = sp.sub(1);
            sp.write(trampoline as *const () as usize);
            // r15, r14, r13, r12, rbx, rbp, popped in that order. r12
            // carries `self`, since it's callee-saved and still there when
            // the trampoline starts.
            for value in [0usize, 0, 0, self_ptr, 0, 0] {
                sp = sp.sub(1);
                sp.write(value);
            }
            self.context.stack_pointer = sp as *mut u8;
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn prepare(&mut self) {
        let top = self.stack.as_ref().expect("a fresh coroutine has its stack").top();
        let self_ptr = self as *mut Coroutine as usize;
        unsafe {
            // 20 saved registers, as the aarch64 switch expects: x19-x28,
            // x29/x30 and d8-d15.
            let sp = (top as *mut usize).sub(20);
            for i in 0..20 {
                sp.add(i).write(0);
            }
            // x19, the first slot and callee-saved, carries `self`. x30
            // (slot 11) is where `ret` jumps, so it holds the trampoline.
            sp.add(0).write(self_ptr);
            sp.add(11).write(trampoline as *const () as usize);
            self.context.stack_pointer = sp as *mut u8;
        }
    }

    /// Runs the coroutine until it yields or finishes, returning its state.
    /// Resuming a `Done` one does nothing.
    pub fn resume(&mut self) -> State {
        if self.state == State::Done {
            return State::Done;
        }
        let mut here = Context::default();
        let previous_return = RETURN_TO.with(|r| r.replace(&mut here as *mut Context));
        let previous_current = CURRENT.with(|c| c.replace(self as *mut Coroutine));
        self.state = State::Running;
        let outer_stack = crate::stackguard::swap_stack(self.stack_top, self.stack_budget);
        // From here until it switches back, execution is on the coroutine's
        // stack.
        unsafe { bish_switch_context(&mut here as *mut Context, &self.context as *const Context) };
        crate::stackguard::swap_stack(outer_stack.0, outer_stack.1);
        RETURN_TO.with(|r| r.set(previous_return));
        CURRENT.with(|c| c.set(previous_current));
        if self.state == State::Running {
            // Back without finishing, so it yielded.
            self.state = State::Suspended;
        }
        self.state
    }

    /// Whether the body ended by panicking.
    pub fn panicked(&self) -> bool {
        self.panicked
    }

    pub fn state(&self) -> State {
        self.state
    }
}

#[cfg(not(target_family = "wasm"))]
impl Drop for Coroutine {
    fn drop(&mut self) {
        // A coroutine dropped mid-yield never runs again, and the values on
        // its stack are never dropped either: leaking them is safe, while
        // running destructors on a stack nothing executes on isn't
        // possible. Callers run coroutines to completion; the scheduler
        // enforces it. The mapping always goes back.
        self.stack.take();
    }
}

/// The first thing that runs on a coroutine's stack: reached by the `ret`
/// ending the first switch, never called.
#[cfg(not(target_family = "wasm"))]
extern "C" fn trampoline() -> ! {
    let co = CURRENT.with(|c| c.get());
    unsafe {
        let body = (*co).body.take();
        if let Some(body) = body {
            // A panic mustn't unwind across a stack switch, onto a stack
            // the unwinder doesn't know. Caught here and reported.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            (*co).panicked = outcome.is_err();
        }
        (*co).state = State::Done;
    }
    // Back to the resumer for the last time; `resume` sees `Done`.
    let back = RETURN_TO.with(|r| r.get());
    let mut discard = Context::default();
    unsafe { bish_switch_context(&mut discard as *mut Context, back as *const Context) };
    unreachable!("a finished coroutine was resumed again");
}

/// Suspends the running coroutine, returning to its resumer. Does nothing
/// outside a coroutine, so code needn't know which it's in.
#[cfg(not(target_family = "wasm"))]
pub fn yield_now() {
    let current = CURRENT.with(|c| c.get());
    let back = RETURN_TO.with(|r| r.get());
    if current.is_null() || back.is_null() {
        return;
    }
    unsafe { bish_switch_context(&mut (*current).context as *mut Context, back as *const Context) };
}

/// Whether this thread is currently inside a coroutine.
#[cfg(not(target_family = "wasm"))]
pub fn in_coroutine() -> bool {
    !CURRENT.with(|c| c.get()).is_null()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn a_coroutine_runs_to_completion() {
        let ran = Rc::new(Cell::new(false));
        let flag = Rc::clone(&ran);
        let mut co = Coroutine::new(move || flag.set(true)).unwrap();
        assert_eq!(co.state(), State::Fresh);
        assert_eq!(co.resume(), State::Done);
        assert!(ran.get(), "the body ran");
        // Resuming a finished one does nothing.
        assert_eq!(co.resume(), State::Done);
    }

    #[test]
    fn a_coroutine_stops_at_a_yield_and_carries_on_from_there() {
        let steps = Rc::new(std::cell::RefCell::new(Vec::new()));
        let s = Rc::clone(&steps);
        let mut co = Coroutine::new(move || {
            s.borrow_mut().push("a");
            yield_now();
            s.borrow_mut().push("b");
            yield_now();
            s.borrow_mut().push("c");
        })
        .unwrap();

        assert_eq!(co.resume(), State::Suspended);
        assert_eq!(*steps.borrow(), ["a"], "stopped at the first yield");
        assert_eq!(co.resume(), State::Suspended);
        assert_eq!(*steps.borrow(), ["a", "b"], "carried on from where it stopped");
        assert_eq!(co.resume(), State::Done);
        assert_eq!(*steps.borrow(), ["a", "b", "c"]);
    }

    // Two making progress in turn on one thread, neither aware of the other.
    #[test]
    fn two_coroutines_interleave() {
        let log = Rc::new(std::cell::RefCell::new(String::new()));
        let mut cos: Vec<Box<Coroutine>> = "AB"
            .chars()
            .map(|name| {
                let log = Rc::clone(&log);
                Coroutine::new(move || {
                    for i in 0..3 {
                        log.borrow_mut().push(name);
                        log.borrow_mut().push_str(&i.to_string());
                        yield_now();
                    }
                })
                .unwrap()
            })
            .collect();
        while cos.iter().any(|c| c.state() != State::Done) {
            for co in cos.iter_mut() {
                if co.state() != State::Done {
                    co.resume();
                }
            }
        }
        assert_eq!(*log.borrow(), "A0B0A1B1A2B2");
    }

    // Each has its own stack, so deep recursion in one disturbs neither the
    // other nor the thread's stack, which `stackguard` measures.
    #[test]
    fn a_coroutine_recurses_on_its_own_stack() {
        fn descend(n: usize) -> usize {
            let ballast = [0u8; 1024];
            std::hint::black_box(&ballast);
            if n == 0 { 0 } else { 1 + descend(n - 1) }
        }
        let depth = Rc::new(Cell::new(0));
        let d = Rc::clone(&depth);
        // 500 frames of 1 KiB: well inside STACK_SIZE, more than a test
        // thread would like.
        let mut co = Coroutine::new(move || d.set(descend(500))).unwrap();
        assert_eq!(co.resume(), State::Done);
        assert_eq!(depth.get(), 500);
    }

    #[test]
    fn yielding_outside_a_coroutine_does_nothing() {
        assert!(!in_coroutine());
        yield_now();
    }

    #[test]
    fn a_coroutine_knows_it_is_one() {
        let inside = Rc::new(Cell::new(false));
        let i = Rc::clone(&inside);
        let mut co = Coroutine::new(move || i.set(in_coroutine())).unwrap();
        co.resume();
        assert!(inside.get());
        assert!(!in_coroutine(), "and the flag is back off afterwards");
    }

    // A panic ends the coroutine and is reported, never unwinding across the
    // switch.
    #[test]
    fn a_panicking_coroutine_ends_and_says_so() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut co = Coroutine::new(|| panic!("from inside")).unwrap();
        let state = co.resume();
        std::panic::set_hook(previous);
        assert_eq!(state, State::Done);
        assert!(co.panicked());
    }

    #[test]
    fn many_switches_stay_correct() {
        let count = Rc::new(Cell::new(0usize));
        let c = Rc::clone(&count);
        let mut co = Coroutine::new(move || {
            for _ in 0..10_000 {
                c.set(c.get() + 1);
                yield_now();
            }
        })
        .unwrap();
        while co.resume() != State::Done {}
        assert_eq!(count.get(), 10_000);
    }
}

#[cfg(test)]
mod cost {
    use super::*;

    // Checks that a switch hasn't become a syscall, as `swapcontext`'s
    // `sigprocmask` would make it. Compared with a real syscall in the same
    // test rather than an absolute time, since this runs alongside thousands
    // of other tests and a ratio moves with load.
    #[test]
    fn a_switch_costs_far_less_than_a_syscall() {
        const N: usize = 100_000;

        // `close(-1)` fails with EBADF: a syscall and nothing else, not one
        // the vDSO answers.
        let started = std::time::Instant::now();
        for _ in 0..N {
            crate::platform::close_fd(-1);
        }
        let per_syscall = started.elapsed().as_nanos() as f64 / N as f64;

        let mut co = Coroutine::new(|| {
            loop {
                yield_now();
            }
        })
        .unwrap();
        let started = std::time::Instant::now();
        for _ in 0..N {
            co.resume();
        }
        // Two switches per resume: in and out.
        let per_switch = started.elapsed().as_nanos() as f64 / (N * 2) as f64;

        assert!(
            per_switch < per_syscall * 3.0,
            "a context switch ({per_switch:.0}ns) should be in the same range as a syscall ({per_syscall:.0}ns), not multiples of it"
        );
        // Dropped while suspended in its loop, which is supported: the
        // mapping goes back, and nothing on that stack needed dropping.
        drop(co);
    }
}
