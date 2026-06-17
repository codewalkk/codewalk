//! `extractSymbolsFromQuery` — identify likely symbol names in an NL query.
//! Port of the function of the same name in `context/index.ts`.

use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

fn common_words() -> &'static HashSet<&'static str> {
    static W: OnceLock<HashSet<&'static str>> = OnceLock::new();
    W.get_or_init(|| {
        [
            "the", "and", "for", "with", "from", "this", "that", "have", "been", "will", "would",
            "could", "should", "does", "done", "make", "made", "use", "used", "using", "work",
            "works", "find", "found", "show", "call", "called", "calling", "get", "set", "add",
            "all", "any", "how", "what", "when", "where", "which", "who", "why", "not", "but",
            "are", "was", "were", "has", "had", "its", "can", "did", "may", "also", "into", "than",
            "then", "them", "each", "other", "some", "such", "only", "same", "about", "after",
            "before", "between", "through", "during", "without", "again", "further", "once", "here",
            "there", "both", "just", "more", "most", "very", "being", "having", "doing", "system",
            "need", "needs", "want", "wants", "like", "look", "change", "changes", "changed",
            "changing", "layer", "handle", "handles", "handling", "incoming", "outgoing", "data",
            "flow", "flows", "level", "levels", "request", "requests", "response", "responses",
            "implement", "implements", "implementation", "interface", "interfaces", "class",
            "classes", "method", "methods", "trigger", "triggers", "affected", "affect", "affects",
            "else", "code", "failing", "failed", "silently", "decide", "decides", "return",
            "returns", "returned", "take", "takes", "taken", "check", "checks", "checked", "create",
            "creates", "created", "read", "reads", "write", "writes", "written", "start", "starts",
            "stop", "stops", "run", "runs", "running",
        ]
        .into_iter()
        .collect()
    })
}

/// Extract likely symbol names from a natural-language query.
pub fn extract_symbols_from_query(query: &str) -> Vec<String> {
    let mut symbols: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let add = |s: &str, symbols: &mut Vec<String>, seen: &mut HashSet<String>| {
        if seen.insert(s.to_string()) {
            symbols.push(s.to_string());
        }
    };

    macro_rules! re {
        ($name:ident, $pat:literal) => {{
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).unwrap())
        }};
    }

    // CamelCase identifiers (2+ chars).
    let camel = re!(camel, r"\b([A-Z][a-z]+(?:[A-Z][a-z]*)*|[a-z]+(?:[A-Z][a-z]*)+)\b");
    for c in camel.captures_iter(query) {
        if let Some(m) = c.get(1) {
            if m.as_str().len() >= 2 {
                add(m.as_str(), &mut symbols, &mut seen);
            }
        }
    }
    // snake_case (case-insensitive, 3+).
    let snake = re!(snake, r"(?i)\b([a-z][a-z0-9]*(?:_[a-z0-9]+)+)\b");
    for c in snake.captures_iter(query) {
        if let Some(m) = c.get(1) {
            if m.as_str().len() >= 3 {
                add(m.as_str(), &mut symbols, &mut seen);
            }
        }
    }
    // SCREAMING_SNAKE_CASE.
    let screaming = re!(screaming, r"\b([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)\b");
    for c in screaming.captures_iter(query) {
        if let Some(m) = c.get(1) {
            add(m.as_str(), &mut symbols, &mut seen);
        }
    }
    // ALL_CAPS acronyms (2+).
    let acronym = re!(acronym, r"\b([A-Z]{2,})\b");
    for c in acronym.captures_iter(query) {
        if let Some(m) = c.get(1) {
            add(m.as_str(), &mut symbols, &mut seen);
        }
    }
    // dot.notation — add full path + parts (2+).
    let dot = re!(dot, r"\b([a-zA-Z][a-zA-Z0-9]*(?:\.[a-zA-Z][a-zA-Z0-9]*)+)\b");
    for c in dot.captures_iter(query) {
        if let Some(m) = c.get(1) {
            add(m.as_str(), &mut symbols, &mut seen);
            for part in m.as_str().split('.') {
                if part.len() >= 2 {
                    add(part, &mut symbols, &mut seen);
                }
            }
        }
    }
    // plain lowercase identifiers (3+).
    let lower = re!(lower, r"\b([a-z][a-z0-9]{2,})\b");
    for c in lower.captures_iter(query) {
        if let Some(m) = c.get(1) {
            add(m.as_str(), &mut symbols, &mut seen);
        }
    }

    symbols
        .into_iter()
        .filter(|s| !common_words().contains(s.to_ascii_lowercase().as_str()))
        .collect()
}
