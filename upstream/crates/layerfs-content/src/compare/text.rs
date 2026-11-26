//! Line difference of two bounded texts, reused by file diffs, merge and blame.
//!
//! Lines are the `\n`-separated pieces of the text, so a trailing newline
//! yields a final empty line, exactly as the SDK simulator splits them. The
//! difference is a longest common subsequence with removals before additions
//! on ties. Its table is bounded by [`MAXIMUM_DIFF_CELLS`]: a larger input keeps
//! its common prefix and suffix and, if the middle still exceeds the bound,
//! reports the middle as one removed-then-added block. No resident buffer
//! grows past the text bound and that table.

/// Largest file, in bytes, that is compared or merged line by line.
pub const MAXIMUM_TEXT_BYTES: u64 = 1 << 20;
/// Largest `(lines + 1) * (lines + 1)` table one difference may hold.
pub const MAXIMUM_DIFF_CELLS: usize = 1 << 20;

/// One line of a difference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineOp {
    /// Present on both sides.
    Same,
    /// Only on the left.
    Removed,
    /// Only on the right.
    Added,
}

/// One line with its operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    /// Operation.
    pub op: LineOp,
    /// Line text without its newline.
    pub text: String,
}

/// The bytes as text: valid UTF-8 with no NUL byte, else `None` (binary).
pub fn as_text(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

/// Splits like the simulator: every `\n` ends a line, the rest is the last.
pub fn lines(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

/// Line difference from `left` to `right`.
pub fn line_diff(left: &str, right: &str) -> Vec<DiffLine> {
    diff_lines(&lines(left), &lines(right))
}

/// Line difference of two already split texts.
pub fn diff_lines(x: &[&str], y: &[&str]) -> Vec<DiffLine> {
    let line = |op, text: &str| DiffLine {
        op,
        text: text.to_owned(),
    };
    if cells(x.len(), y.len()) <= MAXIMUM_DIFF_CELLS {
        return table_diff(x, y);
    }
    let prefix = x.iter().zip(y).take_while(|(a, b)| a == b).count();
    let suffix = x[prefix..]
        .iter()
        .rev()
        .zip(y[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (mx, my) = (&x[prefix..x.len() - suffix], &y[prefix..y.len() - suffix]);
    let mut out: Vec<DiffLine> = x[..prefix].iter().map(|t| line(LineOp::Same, t)).collect();
    if cells(mx.len(), my.len()) <= MAXIMUM_DIFF_CELLS {
        out.extend(table_diff(mx, my));
    } else {
        out.extend(mx.iter().map(|t| line(LineOp::Removed, t)));
        out.extend(my.iter().map(|t| line(LineOp::Added, t)));
    }
    out.extend(x[x.len() - suffix..].iter().map(|t| line(LineOp::Same, t)));
    out
}

fn cells(m: usize, n: usize) -> usize {
    (m + 1).saturating_mul(n + 1)
}

/// The simulator's longest-common-subsequence walk, line for line.
fn table_diff(x: &[&str], y: &[&str]) -> Vec<DiffLine> {
    let (m, n) = (x.len(), y.len());
    let width = n + 1;
    let mut table = vec![0u32; (m + 1) * width];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            table[i * width + j] = if x[i] == y[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(m + n);
    let (mut i, mut j) = (0, 0);
    let push = |out: &mut Vec<DiffLine>, op, text: &str| {
        out.push(DiffLine {
            op,
            text: text.to_owned(),
        })
    };
    while i < m && j < n {
        if x[i] == y[j] {
            push(&mut out, LineOp::Same, x[i]);
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            push(&mut out, LineOp::Removed, x[i]);
            i += 1;
        } else {
            push(&mut out, LineOp::Added, y[j]);
            j += 1;
        }
    }
    for text in &x[i..] {
        push(&mut out, LineOp::Removed, text);
    }
    for text in &y[j..] {
        push(&mut out, LineOp::Added, text);
    }
    out
}

/// For each line of `a`, the index of its matching line of `b`.
pub fn match_lines(a: &[&str], b: &[&str]) -> Vec<Option<usize>> {
    let mut matched = vec![None; a.len()];
    let (mut i, mut j) = (0, 0);
    for line in diff_lines(a, b) {
        match line.op {
            LineOp::Same => {
                matched[i] = Some(j);
                i += 1;
                j += 1;
            }
            LineOp::Removed => i += 1,
            LineOp::Added => j += 1,
        }
    }
    matched
}
