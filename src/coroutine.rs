// The scheduler that drives these, and the pipeline stages that will be
// driven by it, are not written yet -- this is the primitive underneath
// them, landed on its own because it is the part that can be wrong in
// ways only its own tests can catch. Everything here is exercised by
// those tests; the allow goes when the scheduler above it arrives.
#![allow(dead_code)]

// Two interpreters running at once, in one process, on one thread.
//
// A pipeline's stages have to run concurrently -- the producer blocks
// when the pipe fills and cannot continue until the consumer drains it
// -- and a separate process per stage is only *one* way to buy that
// concurrency. It is the expensive way here: a stage that needs a shell
// costs a whole bish startup, and the new process then knows nothing
// this one knows except what a generated preamble tells it.
//
// What is actually needed is the ability to stop one interpreter
// mid-evaluation, run another, and come back. bish's evaluator recurses
// on the native stack, so "where it is" *is* a stack pointer and a
// handful of callee-saved registers -- which makes switching between two
// of them a matter of swapping exactly that, onto a stack of our own.
//
// This module is that switch and nothing else: no scheduler, no I/O, no
// shell. It is the smallest piece that can be tested on its own, and
// everything above it is ordinary safe Rust.
//
// # Why hand-written assembly
//
// There is a libc function for this (`swapcontext`), and it costs about
// a microsecond because it makes a `sigprocmask` syscall on every switch
// to save the signal mask -- which this does not need, since a stage
// never changes it. The switch below touches nothing but registers.
//
// # What is guaranteed
//
// - A coroutine runs until it yields or returns; there is no preemption,
//   so nothing here needs a lock and no state is shared across a switch
//   that was not shared before it.
// - A panic never crosses a switch: the entry point catches it, and the
//   coroutine reports it to whoever resumed it. Unwinding across a stack
//   this module built would be undefined behaviour, and the catch is
//   what makes sure it cannot happen.
// - Each coroutine's stack is its own mapping with an unreadable guard
//   page below it, so overrunning it is a fault at the guard rather than
//   silent corruption of whatever was mapped next.

use std::cell::Cell;

/// The callee-saved state of a suspended execution: everything the
/// System V ABI says a function must preserve, plus where its stack is.
///
/// Only the stack pointer is named here. Everything else lives *on* that
/// stack, pushed by the switch itself -- which is what makes this one
/// word wide and the switch a dozen instructions.
#[repr(C)]
#[derive(Default)]
struct Context {
    stack_pointer: *mut u8,
}

// x86_64 System V: rbx, rbp and r12-r15 are callee-saved, so a function
// that switches stacks has to preserve them across the switch the same
// way any other function would. They are pushed onto the outgoing stack
// and popped from the incoming one; `ret` then returns into whatever
// that stack was last suspended at.
//
// The `ret` at the end is doing the actual jump. A stack prepared by
// `Stack::prepare` has the trampoline's address where a return address
// would be, so the first switch into a coroutine "returns" into it.
#[cfg(target_arch = "x86_64")]
std::arch::global_asm!(
    ".globl bish_switch_context",
    ".hidden bish_switch_context",
    "bish_switch_context:",
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

// aarch64 AAPCS: x19-x28 and the frame/link registers are callee-saved,
// as are the low halves of v8-v15. Same shape as above -- push, swap
// stack pointers, pop, return through the link register.
#[cfg(target_arch = "aarch64")]
std::arch::global_asm!(
    ".globl bish_switch_context",
    ".hidden bish_switch_context",
    "bish_switch_context:",
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

unsafe extern "C" {
    /// Saves the current execution into `from` and resumes `to`.
    ///
    /// Returns to its caller when something switches *back* into
    /// `from` -- so from Rust's point of view this is a call that takes
    /// an arbitrarily long time and may run arbitrary other code, which
    /// is exactly what it is.
    fn bish_switch_context(from: *mut Context, to: *const Context);
}

/// How much stack each coroutine gets.
///
/// bish's evaluator uses several kilobytes per nested construct (see
/// `stackguard`), so this is sized for a stage that recurses rather than
/// for the shallow ones. It is virtual address space, not memory: pages
/// are faulted in as they are touched, so an unused megabyte costs a
/// page-table entry and nothing else.
const STACK_SIZE: usize = 2 * 1024 * 1024;

/// An unreadable page below the stack, so an overrun faults there
/// instead of running into whatever the allocator put next.
const GUARD_SIZE: usize = 4096;

/// One coroutine's stack: `mmap`ed, guarded, and unmapped on drop.
struct Stack {
    base: *mut u8,
    total: usize,
}

impl Stack {
    fn new() -> std::io::Result<Stack> {
        unsafe extern "C" {
            fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
            fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        }
        const PROT_NONE: i32 = 0;
        const PROT_READ: i32 = 1;
        const PROT_WRITE: i32 = 2;
        const MAP_PRIVATE: i32 = 2;
        const MAP_ANONYMOUS: i32 = 0x20;
        const MAP_FAILED: isize = -1;

        let total = GUARD_SIZE + STACK_SIZE;
        let base = unsafe { mmap(std::ptr::null_mut(), total, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
        if base as isize == MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // The guard goes at the *low* end: stacks grow downward, so that
        // is the end an overrun reaches.
        if unsafe { mprotect(base, GUARD_SIZE, PROT_NONE) } != 0 {
            let e = std::io::Error::last_os_error();
            unsafe { unmap(base, total) };
            return Err(e);
        }
        Ok(Stack { base, total })
    }

    /// The highest usable address, aligned down to 16 bytes.
    fn top(&self) -> *mut u8 {
        let top = self.base as usize + self.total;
        (top & !0xf) as *mut u8
    }
}

unsafe fn unmap(base: *mut u8, len: usize) {
    unsafe extern "C" {
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    unsafe { munmap(base, len) };
}

impl Drop for Stack {
    fn drop(&mut self) {
        unsafe { unmap(self.base, self.total) };
    }
}

/// What a coroutine is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Never resumed yet.
    Fresh,
    /// Resumed at least once, and currently suspended in a `yield_now`.
    Suspended,
    /// Currently running -- the one this thread is inside.
    Running,
    /// Returned, or panicked. Resuming again is a no-op.
    Done,
}

thread_local! {
    /// Where to switch back to when the running coroutine yields.
    ///
    /// Set by `resume` immediately before entering the coroutine, so a
    /// `yield_now` from arbitrarily deep inside it knows where "back"
    /// is without having to be handed anything.
    static RETURN_TO: Cell<*mut Context> = const { Cell::new(std::ptr::null_mut()) };
    /// The coroutine this thread is currently inside, so `yield_now`
    /// and the trampoline can find it without being handed anything.
    /// Null when none is running.
    ///
    /// A pointer to the whole `Coroutine` rather than to its `Context`:
    /// deriving one from the other would be assuming a field offset
    /// that Rust does not promise.
    static CURRENT: Cell<*mut Coroutine> = const { Cell::new(std::ptr::null_mut()) };
}

/// A separately-stacked execution that can be stopped and restarted.
pub struct Coroutine {
    context: Context,
    // Dropped last, and only once the coroutine can no longer be
    // resumed -- see `Drop`.
    stack: Option<Stack>,
    state: State,
    // The closure, boxed, until the coroutine's entry point takes it.
    // `None` afterwards, whether it ran or not.
    body: Option<Box<dyn FnOnce()>>,
    // Set if the body panicked, so `resume` can report it rather than
    // letting it cross the switch.
    panicked: bool,
}

impl Coroutine {
    /// Creates a coroutine that will run `body` when first resumed.
    ///
    /// Nothing runs yet: this allocates a stack and arranges for the
    /// first `resume` to land at the top of `body`.
    pub fn new(body: impl FnOnce() + 'static) -> std::io::Result<Box<Coroutine>> {
        let stack = Stack::new()?;
        let mut co =
            Box::new(Coroutine { context: Context::default(), stack: Some(stack), state: State::Fresh, body: Some(Box::new(body)), panicked: false });
        co.prepare();
        Ok(co)
    }

    /// Lays out the initial stack so that switching to it lands in
    /// `trampoline`, with `self` reachable.
    ///
    /// The layout mirrors exactly what `bish_switch_context` pops: the
    /// six saved registers, then a return address. `ret` takes that
    /// address, so the first switch "returns" into the trampoline
    /// having never called it.
    #[cfg(target_arch = "x86_64")]
    fn prepare(&mut self) {
        let top = self.stack.as_ref().expect("a fresh coroutine has its stack").top();
        // System V wants rsp+8 ≡ 0 (mod 16) at a function's first
        // instruction -- the state right after a `call` pushed its
        // return address. `ret` will pop the address below, so the slot
        // holding it plays that part, and everything is measured from
        // an aligned `top`.
        let self_ptr = self as *mut Coroutine as usize;
        unsafe {
            let mut sp = top as *mut usize;
            // A zero return address under the trampoline: if it ever
            // returns rather than switching away, it lands on a null
            // instruction pointer, which faults immediately and
            // visibly instead of wandering.
            sp = sp.sub(1);
            sp.write(0);
            sp = sp.sub(1);
            sp.write(trampoline as *const () as usize);
            // r15, r14, r13, r12, rbx, rbp -- popped in that order by
            // the switch. r12 carries `self` across, since it is
            // callee-saved and therefore still there when the
            // trampoline starts.
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
            // 20 saved registers, matching the aarch64 switch above:
            // x19-x28, x29/x30, and d8-d15.
            let sp = (top as *mut usize).sub(20);
            for i in 0..20 {
                sp.add(i).write(0);
            }
            // x19 is the first slot and is callee-saved: it carries
            // `self`. x30 (the link register, slot 11) is where `ret`
            // jumps, so it holds the trampoline.
            sp.add(0).write(self_ptr);
            sp.add(11).write(trampoline as *const () as usize);
            self.context.stack_pointer = sp as *mut u8;
        }
    }

    /// Runs the coroutine until it yields or finishes.
    ///
    /// Returns the state it is in afterwards. Resuming one that is
    /// already `Done` does nothing.
    pub fn resume(&mut self) -> State {
        if self.state == State::Done {
            return State::Done;
        }
        let mut here = Context::default();
        let previous_return = RETURN_TO.with(|r| r.replace(&mut here as *mut Context));
        let previous_current = CURRENT.with(|c| c.replace(self as *mut Coroutine));
        self.state = State::Running;
        // Everything between here and the line after is running on the
        // coroutine's own stack.
        unsafe { bish_switch_context(&mut here as *mut Context, &self.context as *const Context) };
        RETURN_TO.with(|r| r.set(previous_return));
        CURRENT.with(|c| c.set(previous_current));
        if self.state == State::Running {
            // It came back without finishing, so it yielded.
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

impl Drop for Coroutine {
    fn drop(&mut self) {
        // A coroutine dropped mid-yield never runs again, so whatever it
        // had borrowed on its own stack is never touched again either --
        // but the values living there are also never dropped. Leaking
        // them is the safe half of the trade; running arbitrary
        // destructors on a stack nobody is executing on is not something
        // this can do correctly. Callers run their coroutines to
        // completion; the scheduler above enforces it.
        //
        // The mapping itself always goes back.
        self.stack.take();
    }
}

/// The first thing that runs on a coroutine's own stack.
///
/// `extern "C"` and never called directly: it is reached by the `ret` at
/// the end of the first switch into this stack.
extern "C" fn trampoline() -> ! {
    let co = CURRENT.with(|c| c.get());
    unsafe {
        let body = (*co).body.take();
        if let Some(body) = body {
            // A panic must not unwind across a stack switch: the
            // unwinder would walk off the end of a stack it knows
            // nothing about. Caught here, reported through the struct.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            (*co).panicked = outcome.is_err();
        }
        (*co).state = State::Done;
    }
    // Back to whoever resumed this, for the last time. `resume` sees
    // `Done` and does not come back.
    let back = RETURN_TO.with(|r| r.get());
    let mut discard = Context::default();
    unsafe { bish_switch_context(&mut discard as *mut Context, back as *const Context) };
    unreachable!("a finished coroutine was resumed again");
}

/// Suspends the running coroutine, returning to whoever resumed it.
///
/// A no-op when nothing is running as a coroutine, so code that can run
/// either way does not have to know which it is.
pub fn yield_now() {
    let current = CURRENT.with(|c| c.get());
    let back = RETURN_TO.with(|r| r.get());
    if current.is_null() || back.is_null() {
        return;
    }
    unsafe { bish_switch_context(&mut (*current).context as *mut Context, back as *const Context) };
}

/// Whether this thread is currently inside a coroutine.
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
        // Resuming a finished one is a no-op rather than a fault.
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

    // The property the whole thing exists for: two of them making
    // progress in turn, on one thread, neither aware of the other.
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

    // Each one is on its own stack, so a deep recursion in one must not
    // disturb the other -- and must not disturb the thread's own stack
    // either, which is what `stackguard` measures against.
    #[test]
    fn a_coroutine_recurses_on_its_own_stack() {
        fn descend(n: usize) -> usize {
            let ballast = [0u8; 1024];
            std::hint::black_box(&ballast);
            if n == 0 { 0 } else { 1 + descend(n - 1) }
        }
        let depth = Rc::new(Cell::new(0));
        let d = Rc::clone(&depth);
        // 500 frames of 1KiB each, comfortably inside STACK_SIZE and
        // comfortably more than a test harness thread would like.
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

    // A panic must not unwind across the switch -- it would walk off a
    // stack the unwinder knows nothing about. It ends the coroutine
    // instead, and is reported.
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

    // Not an assertion about speed -- a floor check, so that a switch
    // silently becoming a syscall would show up. `swapcontext`, the
    // libc equivalent, is around a microsecond because it saves the
    // signal mask; this must be nowhere near that.
    #[test]
    fn a_switch_costs_far_less_than_a_syscall() {
        const N: usize = 200_000;
        let mut co = Coroutine::new(|| {
            loop {
                yield_now();
            }
        })
        .unwrap();
        let start = std::time::Instant::now();
        for _ in 0..N {
            co.resume();
        }
        // Two switches per resume: in and back out.
        let per_switch = start.elapsed().as_nanos() as f64 / (N * 2) as f64;
        eprintln!("[coroutine] {per_switch:.1}ns per switch");
        assert!(per_switch < 500.0, "a context switch should be tens of nanoseconds, not {per_switch:.1}ns");
        // Dropped while still suspended inside its loop, which is the
        // supported thing to do: the mapping goes back, and nothing on
        // that stack owns anything that needed dropping.
        drop(co);
    }
}
