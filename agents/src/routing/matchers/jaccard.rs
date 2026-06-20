use std::collections::HashSet;

pub fn score(query_tokens: &[String], corpus_tokens: &[String]) -> f64 {
    if query_tokens.is_empty() || corpus_tokens.is_empty() {
        return 0.0;
    }
    let q = query_tokens
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let c = corpus_tokens
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let intersection = q.intersection(&c).count() as f64;
    let union = q.union(&c).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_overlap() {
        let a = vec!["auth".to_string(), "token".to_string()];
        let b = vec!["token".to_string(), "route".to_string()];
        assert!(score(&a, &b) > 0.0);
    }
}
