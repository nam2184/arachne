/// Wildcard matching for permission rules:
/// `*` matches zero or more of any character
/// `?` matches exactly one character
/// All other characters match literally
///
/// A trailing ` *` (literal space + star) is treated as optional, so the
/// pattern `git *` matches both `git` and `git status`.
pub fn wildcard_match(pattern: &str, value: &str) -> bool {
    wildcard_match_inner(pattern.as_bytes(), value.as_bytes())
}

fn wildcard_match_inner(pattern: &[u8], value: &[u8]) -> bool {
    let mut p = 0usize;
    let mut v = 0usize;
    let mut star: Option<usize> = None;
    let mut match_pos: usize = 0;

    while v < value.len() {
        if p < pattern.len() && pattern[p] == b'?' {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            match_pos = v;
            p += 1;
        } else if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if let Some(s) = star {
            // Backtrack: let the last star consume one more char.
            p = s + 1;
            match_pos += 1;
            v = match_pos;
        } else {
            return false;
        }
    }

    // Value exhausted. Pattern must either be exhausted too, or contain only
    // trailing stars (which match zero characters).
    if p == pattern.len() {
        return true;
    }
    if pattern[p..].iter().all(|c| *c == b'*') {
        return true;
    }
    // Special case: a trailing ` *` (space + star) is optional,
    // so `git *` matches `git` even though the literal space is in the pattern.
    if p + 1 < pattern.len()
        && pattern[p] == b' '
        && pattern[p + 1] == b'*'
        && pattern[p + 1..].iter().all(|c| *c == b'*')
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pattern_matches_empty_value() {
        assert!(wildcard_match("", ""));
    }

    #[test]
    fn empty_pattern_does_not_match_value() {
        assert!(!wildcard_match("", "x"));
    }

    #[test]
    fn literal_match() {
        assert!(wildcard_match("git", "git"));
        assert!(!wildcard_match("git", "got"));
    }

    #[test]
    fn star_matches_zero_or_more() {
        assert!(wildcard_match("git *", "git"));
        assert!(wildcard_match("git *", "git status"));
        assert!(wildcard_match("git *", "git commit -m foo"));
        assert!(!wildcard_match("git *", "got status"));
    }

    #[test]
    fn star_alone_matches_everything() {
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("*", "anything"));
    }

    #[test]
    fn question_matches_one_char() {
        assert!(wildcard_match("g?t", "git"));
        assert!(wildcard_match("g?t", "got"));
        assert!(!wildcard_match("g?t", "gt"));
        assert!(!wildcard_match("g?t", "goot"));
    }

    #[test]
    fn multiple_stars() {
        assert!(wildcard_match("**/*.rs", "src/main.rs"));
        assert!(wildcard_match("**/*.rs", "a/b/c/d.rs"));
        assert!(!wildcard_match("**/*.rs", "src/main.go"));
    }

    #[test]
    fn star_at_end() {
        assert!(wildcard_match("rm *", "rm -rf /"));
        assert!(wildcard_match("rm *", "rm"));
    }

    #[test]
    fn star_at_start() {
        assert!(wildcard_match("*.env", ".env"));
        assert!(wildcard_match("*.env", "production.env"));
        assert!(!wildcard_match("*.env", "env"));
    }

    #[test]
    fn home_expansion_not_handled_here() {
        // The '~' / '$HOME' expansion is the responsibility of the config loader,
        // not the matcher. Just verify literal tilde doesn't surprise us.
        assert!(!wildcard_match("~/*", "/home/user/foo"));
        assert!(wildcard_match("~/*", "~/.bashrc"));
    }

    #[test]
    fn star_must_be_present_for_zero_match() {
        // Without a star, the entire pattern must match.
        assert!(!wildcard_match("git", ""));
    }

    #[test]
    fn trailing_optional_space_star() {
        // `git *` matches both `git` and `git status`.
        assert!(wildcard_match("git *", "git"));
        assert!(wildcard_match("git *", "git status"));
        assert!(!wildcard_match("git *", "got status"));
    }
}
