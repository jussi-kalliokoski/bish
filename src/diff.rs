// Myers' O(ND) diff ("An O(ND) Difference Algorithm and Its Variations",
// 1986), the algorithm diff and git use.
//
// Generic over `&[T]` of `PartialEq` elements, so the caller decides what a
// line is: real lines, or chars for a finer diff. Produces the shortest
// edit script as an in-order run of `DiffOp`s. O((N+M)D) time and memory,
// D being the edit script's size: fine for scripts and configs, slow for
// large, very different inputs, as `git diff` is.

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

// One step of the edit script before `coalesce` merges runs: each covers
// one element, unlike `DiffOp`.
enum Step {
    Equal,
    Delete,
    Insert,
}

// The V array at the start of each round `d` of the greedy search:
// `trace[d]` is the frontier reachable in `d - 1` edits, which `backtrack`
// walks back through. The edit script can't exceed `len(a) + len(b)`
// (delete all, insert all), which bounds the loop and the array.
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

// Walks `trace` from (a.len(), b.len()) back to (0, 0) a round at a time,
// one `Step` per element, then reverses into forward order.
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
            // Exactly one fires: a move along x deletes from `a`, along y
            // inserts from `b`.
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

// Merges same-kind Steps into runs, tracking each run's start in `a` and
// `b`.
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
        // Replaying the script against `a` gives `b`: the property that
        // matters, rather than one of several equally short scripts.
        assert_eq!(replay(&a, &b, &ops), b);
    }

    #[test]
    fn a_realistic_multi_line_edit_reconstructs_correctly() {
        let a = lines("fn main() {\n    println!(\"hello\");\n}\n");
        let b = lines("fn main() {\n    println!(\"hello, world\");\n    println!(\"bye\");\n}\n");
        let ops = diff(&a, &b);
        assert_eq!(replay(&a, &b, &ops), b);
    }

    // Replays a script against `a`, also checking each Equal run against
    // `b`, which is what proves the script correct.
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
