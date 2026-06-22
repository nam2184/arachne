pub fn tokenize(text: &str, stem: bool) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut prev: Option<String> = None;
    for raw in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        for word in split_identifier(raw) {
            let mut token = word.to_ascii_lowercase();
            if token.len() < 3 || is_stop_word(&token) {
                continue;
            }
            if stem {
                token = cheap_stem(&token);
            }
            if token.len() < 3 || is_stop_word(&token) {
                continue;
            }
            if let Some(previous) = &prev {
                tokens.push(format!("{previous}_{token}"));
            }
            prev = Some(token.clone());
            tokens.push(token);
        }
    }
    tokens
}

fn split_identifier(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for ch in input.chars() {
        if ch == '_' || ch == '-' {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_lower && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn cheap_stem(token: &str) -> String {
    for suffix in [
        "ization", "ation", "ments", "ment", "ing", "ers", "ies", "ed", "s",
    ] {
        if token.len() > suffix.len() + 3 && token.ends_with(suffix) {
            return token[..token.len() - suffix.len()].to_string();
        }
    }
    token.to_string()
}

fn is_stop_word(token: &str) -> bool {
    matches!(
        token,
        "the"
            | "and"
            | "for"
            | "with"
            | "from"
            | "that"
            | "this"
            | "then"
            | "else"
            | "when"
            | "while"
            | "where"
            | "what"
            | "which"
            | "your"
            | "their"
            | "about"
            | "into"
            | "onto"
            | "over"
            | "under"
            | "true"
            | "false"
            | "none"
            | "some"
            | "return"
            | "const"
            | "static"
            | "public"
            | "private"
            | "protected"
            | "async"
            | "await"
            | "impl"
            | "trait"
            | "enum"
            | "struct"
            | "function"
            | "class"
            | "self"
            | "super"
            | "crate"
            | "mod"
            | "pub"
            | "let"
            | "var"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_camel_and_bigrams() {
        let tokens = tokenize("RefreshToken rotation", true);
        assert!(tokens.contains(&"refresh".to_string()));
        assert!(tokens.contains(&"token".to_string()));
        assert!(tokens.iter().any(|token| token.contains("refresh_token")));
    }
}
