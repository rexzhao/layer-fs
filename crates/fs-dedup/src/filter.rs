//! Case-insensitive path & name filter rules for `import_exclude` and `no_dedup`.
//!
//! Supported pattern forms (all case-insensitive, `/` and `\` treated identically):
//! - Plain name / glob without `/` (e.g. `Temp`, `Logs`, `*.tmp`):
//!   matches any single path component anywhere in the tree.
//! - Relative path / glob with `/` (e.g. `Library/ShaderCache`, `*/Temp`, `Assets/*/Temp`):
//!   matches the full relative path OR any trailing path suffix aligned on a `/` boundary
//!   (so `Library/ShaderCache` matches both `Library/ShaderCache` and
//!   `evol_release_261119/Library/ShaderCache`!).

/// Curated Unity preset patterns for `import_exclude` (ephemeral build/temp caches).
pub const UNITY_IMPORT_EXCLUDE_PRESET: &[&str] = &[
    "Temp",
    "Logs",
    "Library/ShaderCache",
    "Library/StateCache",
    "Library/Il2cppBuildCache",
    "Library/Bee",
    "Library/TempArtifacts",
    "obj",
];

/// Curated Unity preset patterns for `no_dedup` (hot runtime directories/files that remain
/// writable as local NTFS sparse files and skip background pack deduplication).
pub const UNITY_NO_DEDUP_PRESET: &[&str] = &[
    "Temp",
    "Logs",
    "Library/ShaderCache",
    "Library/StateCache",
    "Library/Il2cppBuildCache",
    "Library/Bee",
    "Library/TempArtifacts",
    "obj",
    ".vs",
];

/// Normalizes a pattern or relative path: lowercase ASCII, `/` separators, no leading/trailing `/`.
pub fn normalize_pattern(raw: &str) -> String {
    raw.trim()
        .replace('\\', "/")
        .trim_matches('/')
        .to_ascii_lowercase()
}

/// Simple `*` and `?` wildcard matcher on ASCII-lowercased strings (`*` does NOT cross `/`).
fn glob_match_segment(pat: &str, text: &str) -> bool {
    let p = pat.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            pi += 1;
            star_t = ti;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Returns `true` if `rel_path` (e.g. `"evol_release_261119/Temp/UnityLockfile"`) matches `raw_pattern`.
///
/// A match on any ancestor directory component or path prefix also matches its descendants
/// (e.g. pattern `"Temp"` or `"Library/ShaderCache"` matches `"evol_261119/Temp/foo.bin"`).
pub fn path_matches(rel_path: &str, raw_pattern: &str) -> bool {
    let pat = normalize_pattern(raw_pattern);
    if pat.is_empty() {
        return false;
    }
    let path = normalize_pattern(rel_path);
    if path.is_empty() {
        return false;
    }
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let pat_segs: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
    if pat_segs.is_empty() || pat_segs.len() > path_segs.len() {
        return false;
    }

    // Slide `pat_segs` across every contiguous sub-slice of `path_segs`.
    // For single-segment patterns like `Temp` or `*.tmp`, this checks every component.
    // For multi-segment patterns like `Library/ShaderCache` or `*/Temp`, this matches
    // both at workspace root AND under a nested worktree folder (`evol_261119/Library/ShaderCache/...`).
    let max_start = path_segs.len() - pat_segs.len();
    for start in 0..=max_start {
        let mut all_ok = true;
        for (offset, pseg) in pat_segs.iter().enumerate() {
            if !glob_match_segment(pseg, path_segs[start + offset]) {
                all_ok = false;
                break;
            }
        }
        if all_ok {
            return true;
        }
    }
    false
}

/// Returns `true` if `rel_path` matches any pattern in `patterns`.
pub fn matches_any(rel_path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| path_matches(rel_path, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_single_and_multi_segment_unity_paths() {
        assert!(path_matches("Temp", "Temp"));
        assert!(path_matches("Temp/UnityLockfile", "Temp"));
        assert!(path_matches("evol_release_261119/Temp/foo.dat", "Temp"));
        assert!(path_matches("evol_release_261119/temp/foo.dat", "Temp"));
        assert!(!path_matches("Assets/Template/hero.prefab", "Temp"));

        assert!(path_matches(
            "evol_release_261119/Library/ShaderCache/a/b",
            "Library/ShaderCache"
        ));
        assert!(!path_matches(
            "evol_release_261119/Assets/ShaderCache/a",
            "Library/ShaderCache"
        ));

        assert!(path_matches("Library/TempArtifacts/123", "Library/Temp*"));
        assert!(path_matches("Assets/foo.tmp", "*.tmp"));
        assert!(!path_matches("Assets/foo.txt", "*.tmp"));
    }
}
