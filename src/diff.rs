// Myers' O(ND) diff algorithm ("An O(ND) Difference Algorithm and Its
// Variations", Eugene Myers, 1986) -- the same algorithm real `diff`,
// git, and most diff libraries use under the hood. No external crate
// (this is exactly the kind of thing a `similar`/`difflib` crate would
// normally be reached for), same spirit as glob.rs/regex.rs/csscolor.rs/
// json.rs.
//
// Operates generically over `&[T]` (`PartialEq`), one element per
// "line" from the caller's own perspective -- callers diffing text
// split it into lines themselves first, this module has no opinion on
// what a "line" is (so it works equally well for a `Vec<&str>` of real
// lines or a `Vec<char>` of individual characters, for a finer-grained
// diff). Produces the shortest edit script as a flat, in-order sequence
// of `DiffOp`.
//
// Standard algorithmic complexity for this algorithm: O((N+M)*D) time
// and O((N+M)*D) memory, where D is the size of the actual edit script
// (small for two mostly-similar files, up to N+M for two completely
// different ones) -- the same cost every other Myers-diff
// implementation has, not a shortcut taken here. Fine for the file/
// buffer sizes this project's editor actually targets (scripts/
// configs, not multi-megabyte generated files); a real, very large,
// very different pair of inputs would be slow the same way it would be
// for `git diff` itself on the same inputs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    /// `a[a..a+len]` and `b[b..b+len]` are identical runs.
    Equal { a: usize, b: usize, len: usize },
    /// `a[a..a+len]` was removed (no counterpart in `b`).
    Delete { a: usize, len: usize },
    /// `b[b..b+len]` was inserted (no counterpart in `a`).
    Insert { b: usize, len: usize },
}

pub fn diff<T: PartialEq>(a: &[T], b: &[T]) -> Vec<DiffOp> {
    let trace = myers_trace(a, b);
    let moves = backtrack(a.len(), b.len(), &trace);
    coalesce(moves)
}

// One step of the backtracked edit script, before adjacent steps of the
// same kind get merged into runs (see `coalesce`) -- `Equal`/`Delete`/
// `Insert` here always cover exactly one element, unlike the public
// `DiffOp` (which covers a whole run).
enum Step {
    Equal,
    Delete,
    Insert,
}

// The V-array snapshot taken at the start of every "round" `d` (0..=
// max edits) of Myers' own greedy search -- `trace[d]` is the frontier
// reached using at most `d-1` edits, needed by `backtrack` to walk
// back from the end to the start one round at a time. `max` diagonals
// are impossible to overflow past (the shortest edit script can never
// exceed len(a)+len(b): delete everything, then insert everything),
// which is what bounds the outer loop and the V array's own width.
fn myers_trace<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Vec<i64>> {
    let n = a.len() as i64;
    let m = b.len() as i64;
    let max = n + m;
    let mut trace: Vec<Vec<i64>> = Vec::new();
    if max == 0 {
        return trace;
    }
    let width = 2 * max as usize + 1;
    let offset = max as usize;
    let idx = |k: i64| (k + offset as i64) as usize;
    let mut v: Vec<i64> = vec![0; width];
    let mut d = 0;
    loop {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) { v[idx(k + 1)] } else { v[idx(k - 1)] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx(k)] = x;
            if x >= n && y >= m {
                return trace;
            }
            k += 2;
        }
        d += 1;
    }
}

// Walks `trace` from the end (a.len(), b.len()) back to the start
// (0, 0), one round at a time, emitting one `Step` per element moved
// along the way -- in *reverse* order (last edit first), flipped back
// to forward order before returning.
fn backtrack(n: usize, m: usize, trace: &[Vec<i64>]) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut x = n as i64;
    let mut y = m as i64;
    let max = (n + m) as i64;
    let offset = max as usize;
    let idx = |k: i64| (k + offset as i64) as usize;
    for d in (0..trace.len()).rev() {
        let d = d as i64;
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) { k + 1 } else { k - 1 };
        let prev_x = v[idx(prev_k)];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            steps.push(Step::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            // Exactly one of these fires: a horizontal move (x changed,
            // y didn't) is a deletion from `a`; a vertical move is an
            // insertion from `b`.
            if x > prev_x {
                steps.push(Step::Delete);
            } else {
                steps.push(Step::Insert);
            }
        }
        x = prev_x;
        y = prev_y;
    }
    steps.reverse();
    steps
}

// Merges consecutive same-kind Steps into runs, tracking each run's own
// starting index into `a`/`b` as it goes.
fn coalesce(steps: Vec<Step>) -> Vec<DiffOp> {
    let mut out: Vec<DiffOp> = Vec::new();
    let mut ai = 0usize;
    let mut bi = 0usize;
    for step in steps {
        match step {
            Step::Equal => {
                match out.last_mut() {
                    Some(DiffOp::Equal { len, .. }) => *len += 1,
                    _ => out.push(DiffOp::Equal { a: ai, b: bi, len: 1 }),
                }
                ai += 1;
                bi += 1;
            }
            Step::Delete => {
                match out.last_mut() {
                    Some(DiffOp::Delete { len, .. }) => *len += 1,
                    _ => out.push(DiffOp::Delete { a: ai, len: 1 }),
                }
                ai += 1;
            }
            Step::Insert => {
                match out.last_mut() {
                    Some(DiffOp::Insert { len, .. }) => *len += 1,
                    _ => out.push(DiffOp::Insert { b: bi, len: 1 }),
                }
                bi += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    #[test]
    fn identical_sequences_are_one_big_equal_run() {
        let a = lines("one\ntwo\nthree");
        let ops = diff(&a, &a.clone());
        assert_eq!(ops, vec![DiffOp::Equal { a: 0, b: 0, len: 3 }]);
    }

    #[test]
    fn both_empty_is_no_ops_at_all() {
        let a: Vec<&str> = Vec::new();
        let ops = diff(&a, &a);
        assert!(ops.is_empty());
    }

    #[test]
    fn empty_a_is_a_pure_insert() {
        let a: Vec<&str> = Vec::new();
        let b = lines("one\ntwo");
        assert_eq!(diff(&a, &b), vec![DiffOp::Insert { b: 0, len: 2 }]);
    }

    #[test]
    fn empty_b_is_a_pure_delete() {
        let a = lines("one\ntwo");
        let b: Vec<&str> = Vec::new();
        assert_eq!(diff(&a, &b), vec![DiffOp::Delete { a: 0, len: 2 }]);
    }

    #[test]
    fn a_single_line_changed_in_the_middle() {
        let a = lines("one\ntwo\nthree");
        let b = lines("one\nTWO\nthree");
        assert_eq!(
            diff(&a, &b),
            vec![
                DiffOp::Equal { a: 0, b: 0, len: 1 },
                DiffOp::Delete { a: 1, len: 1 },
                DiffOp::Insert { b: 1, len: 1 },
                DiffOp::Equal { a: 2, b: 2, len: 1 },
            ]
        );
    }

    #[test]
    fn a_line_inserted_in_the_middle() {
        let a = lines("one\nthree");
        let b = lines("one\ntwo\nthree");
        assert_eq!(diff(&a, &b), vec![DiffOp::Equal { a: 0, b: 0, len: 1 }, DiffOp::Insert { b: 1, len: 1 }, DiffOp::Equal { a: 1, b: 2, len: 1 }]);
    }

    #[test]
    fn a_line_removed_from_the_middle() {
        let a = lines("one\ntwo\nthree");
        let b = lines("one\nthree");
        assert_eq!(diff(&a, &b), vec![DiffOp::Equal { a: 0, b: 0, len: 1 }, DiffOp::Delete { a: 1, len: 1 }, DiffOp::Equal { a: 2, b: 1, len: 1 }]);
    }

    #[test]
    fn completely_different_sequences_reconstruct_correctly() {
        let a = lines("aaa\nbbb\nccc");
        let b = lines("xxx\nyyy");
        let ops = diff(&a, &b);
        // Whatever the exact edit script shape, replaying it against
        // `a` must reconstruct `b` exactly -- the property that
        // actually matters, checked directly rather than pinning down
        // one specific (of several equally-short) edit scripts.
        assert_eq!(replay(&a, &b, &ops), b);
    }

    #[test]
    fn a_realistic_multi_line_edit_reconstructs_correctly() {
        let a = lines("fn main() {\n    println!(\"hello\");\n}\n");
        let b = lines("fn main() {\n    println!(\"hello, world\");\n    println!(\"bye\");\n}\n");
        let ops = diff(&a, &b);
        assert_eq!(replay(&a, &b, &ops), b);
    }

    // Replays an edit script against `a`, asserting every Equal run
    // actually matches the corresponding slice of `b` too (not just
    // `a`) along the way -- this is what actually proves the script is
    // correct, not just shaped right.
    fn replay<'a>(a: &[&'a str], b: &[&'a str], ops: &[DiffOp]) -> Vec<&'a str> {
        let mut out = Vec::new();
        for op in ops {
            match *op {
                DiffOp::Equal { a: ai, b: bi, len } => {
                    assert_eq!(&a[ai..ai + len], &b[bi..bi + len]);
                    out.extend_from_slice(&a[ai..ai + len]);
                }
                DiffOp::Delete { .. } => {}
                DiffOp::Insert { b: bi, len } => out.extend_from_slice(&b[bi..bi + len]),
            }
        }
        out
    }
}
