//! Search query utilities — port of `codegraph/src/search/query-utils.ts`.
//!
//! Pure functions for term extraction and lexical scoring used by the context
//! builder and the DB search layer. No vectors — this is the commodity lexical
//! ranking that `buildContext` is built on.

use crate::types::NodeKind;
use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

/// Normalize a name to a comparable token: lowercase, alphanumerics only.
pub fn normalize_name_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Common stop words to filter from search queries (English + code noise).
/// Mirrors `STOP_WORDS` in query-utils.ts.
pub fn stop_words() -> &'static HashSet<&'static str> {
    static SW: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SW.get_or_init(|| {
        [
            "the", "a", "an", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with",
            "by", "from", "is", "it", "that", "this", "are", "was", "be", "has", "had", "have",
            "do", "does", "did", "will", "would", "could", "should", "may", "might", "can",
            "shall", "not", "no", "all", "each", "every", "how", "what", "where", "when", "who",
            "which", "why", "i", "me", "my", "we", "our", "you", "your", "he", "she", "they",
            "show", "give", "tell", "been", "done", "made", "used", "using", "work", "works",
            "found", "also", "into", "then", "than", "just", "more", "some", "such", "over",
            "only", "out", "its", "so", "up", "as", "if", "look", "need", "needs", "want",
            "happen", "happens", "affect", "affected", "break", "breaks", "failing",
            "implemented", "implement", "code", "file", "files", "function", "method", "class",
            "type", "fix", "bug", "called",
        ]
        .into_iter()
        .collect()
    })
}

fn is_stop_word(w: &str) -> bool {
    stop_words().contains(w)
}

/// Generate stem variants of a term by removing common English suffixes.
/// Used for FTS prefix expansion ("caching" → "cache"). Port of `getStemVariants`.
pub fn get_stem_variants(term: &str) -> Vec<String> {
    let mut variants: HashSet<String> = HashSet::new();
    let t = term.to_ascii_lowercase();
    let chars: Vec<char> = t.chars().collect();
    let len = chars.len();
    let take = |n: usize| -> String { chars[..n].iter().collect() };
    let last_two_equal = |base: &[char]| -> bool {
        base.len() >= 2 && base[base.len() - 1] == base[base.len() - 2]
    };

    // -ing
    if t.ends_with("ing") && len > 5 {
        let base = take(len - 3);
        let bchars: Vec<char> = base.chars().collect();
        variants.insert(base.clone());
        variants.insert(format!("{}e", base));
        if last_two_equal(&bchars) {
            variants.insert(bchars[..bchars.len() - 1].iter().collect());
        }
    }
    // -tion / -sion
    if (t.ends_with("tion") || t.ends_with("sion")) && len > 5 {
        variants.insert(take(len - 3));
    }
    // -ment
    if t.ends_with("ment") && len > 6 {
        variants.insert(take(len - 4));
    }
    // -ies → -y, else -es, else -s
    if t.ends_with("ies") && len > 4 {
        variants.insert(format!("{}y", take(len - 3)));
    } else if t.ends_with("es") && len > 4 {
        variants.insert(take(len - 2));
    } else if t.ends_with('s') && !t.ends_with("ss") && len > 4 {
        variants.insert(take(len - 1));
    }
    // -ed
    if t.ends_with("ed") && !t.ends_with("eed") && len > 4 {
        variants.insert(take(len - 1));
        variants.insert(take(len - 2));
        if t.ends_with("ied") && len > 5 {
            variants.insert(format!("{}y", take(len - 3)));
        }
    }
    // -er
    if t.ends_with("er") && len > 4 {
        let base = take(len - 2);
        let bchars: Vec<char> = base.chars().collect();
        variants.insert(base.clone());
        variants.insert(format!("{}e", base));
        if last_two_equal(&bchars) {
            variants.insert(bchars[..bchars.len() - 1].iter().collect());
        }
    }

    variants
        .into_iter()
        .filter(|v| v.chars().count() >= 3 && *v != t)
        .collect()
}

/// Insert spaces at camelCase / PascalCase / acronym boundaries.
/// `getUserName` → `get User Name`, `RPCProtocol` → `RPC Protocol`.
fn camel_space(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    for i in 0..chars.len() {
        let c = chars[i];
        if i > 0 {
            let prev = chars[i - 1];
            // lower→Upper
            let lower_upper = prev.is_ascii_lowercase() && c.is_ascii_uppercase();
            // UPPER→Upper+lower (acronym boundary): ABCDef → ABC Def
            let acronym = prev.is_ascii_uppercase()
                && c.is_ascii_uppercase()
                && i + 1 < chars.len()
                && chars[i + 1].is_ascii_lowercase();
            if lower_upper || acronym {
                out.push(' ');
            }
        }
        out.push(c);
    }
    out
}

/// Split an identifier into space-joined lowercased tokens for the FTS
/// `name_split` column (CBM's `cbm_camel_split`): `getUserById` → "get user by
/// id", `MAX_RETRIES` → "max retries". Order-preserving, includes short tokens
/// (`by`, `id`) so a query term matches them. Returns "" when nothing splits.
pub fn camel_split(name: &str) -> String {
    let spaced = camel_space(name);
    let normalised: String = spaced
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect();
    let toks: Vec<&str> = normalised
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .collect();
    // Only emit a split when it actually differs from the single lowercased name
    // (avoids storing a redundant copy for already-atomic names).
    if toks.len() <= 1 {
        return String::new();
    }
    toks.join(" ")
}

/// Extract meaningful search terms from a natural-language query.
/// Splits camelCase/snake_case/dot.notation, drops stop words, optionally adds
/// stem variants. Port of `extractSearchTerms`.
pub fn extract_search_terms(query: &str, include_stems: bool) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push = |t: String, tokens: &mut Vec<String>, seen: &mut HashSet<String>| {
        if seen.insert(t.clone()) {
            tokens.push(t);
        }
    };

    // Preserve compound identifiers (camelCase / PascalCase) before splitting.
    for w in query.split(|c: char| !(c.is_ascii_alphanumeric())) {
        if w.len() >= 3 && has_internal_camel(w) {
            push(w.to_ascii_lowercase(), &mut tokens, &mut seen);
        }
    }
    // snake_case compounds.
    for w in query.split(|c: char| c.is_whitespace() || "()[]{}.,;:\"'".contains(c)) {
        if w.len() >= 3 && w.contains('_') && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            push(w.to_ascii_lowercase(), &mut tokens, &mut seen);
        }
    }

    // camelCase-split then split on non-alphanumerics.
    let spaced = camel_space(query);
    let normalised: String = spaced
        .chars()
        .map(|c| if c == '_' || c == '.' { ' ' } else { c })
        .collect();
    for word in normalised.split(|c: char| !c.is_ascii_alphanumeric()) {
        let lower = word.to_ascii_lowercase();
        if lower.len() < 3 || is_stop_word(&lower) {
            continue;
        }
        push(lower, &mut tokens, &mut seen);
    }

    if include_stems {
        let mut stems: Vec<String> = Vec::new();
        for token in &tokens {
            for v in get_stem_variants(token) {
                if !seen.contains(&v) && !is_stop_word(&v) {
                    if seen.insert(v.clone()) {
                        stems.push(v);
                    }
                }
            }
        }
        tokens.extend(stems);
    }
    tokens
}

/// True if `w` has an internal uppercase boundary (camelCase or PascalCase with
/// >1 hump) — i.e. matches the TS compound-identifier patterns.
fn has_internal_camel(w: &str) -> bool {
    let chars: Vec<char> = w.chars().collect();
    if !chars[0].is_ascii_alphabetic() {
        return false;
    }
    let mut humps = 0;
    for i in 1..chars.len() {
        if chars[i].is_ascii_uppercase() && chars[i - 1].is_ascii_lowercase() {
            humps += 1;
        }
    }
    // camelCase: starts lower, ≥1 hump; PascalCase: starts upper, ≥1 internal hump.
    humps >= 1
}

/// Score a file path's relevance to a query. Port of `scorePathRelevance`.
pub fn score_path_relevance(file_path: &str, query: &str, project_name_tokens: &HashSet<String>) -> i64 {
    let path_lower = file_path.to_ascii_lowercase();
    let file_name = Path::new(file_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let dir_name = Path::new(file_path)
        .parent()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut score = 0i64;

    let all_words: Vec<&str> = query.split_whitespace().filter(|w| !w.is_empty()).collect();
    if all_words.is_empty() {
        return 0;
    }
    let words: Vec<&str> = if !project_name_tokens.is_empty() {
        let filtered: Vec<&str> = all_words
            .iter()
            .copied()
            .filter(|w| !project_name_tokens.contains(&normalize_name_token(w)))
            .collect();
        if filtered.is_empty() {
            all_words.clone()
        } else {
            filtered
        }
    } else {
        all_words.clone()
    };

    for word in &words {
        let subtokens = extract_search_terms(word, false);
        if subtokens.is_empty() {
            continue;
        }
        if subtokens.iter().any(|t| file_name.contains(t.as_str())) {
            score += 10;
        }
        if subtokens.iter().any(|t| dir_name.contains(t.as_str())) {
            score += 5;
        } else if subtokens.iter().any(|t| path_lower.contains(t.as_str())) {
            score += 3;
        }
    }

    let q_lower = query.to_ascii_lowercase();
    let is_test_query = q_lower.contains("test") || q_lower.contains("spec");
    if !is_test_query && is_test_file(file_path) {
        score -= 15;
    }
    score
}

/// Whether a file path looks like a test/non-production file. Port of `isTestFile`.
pub fn is_test_file(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    let file_name = Path::new(file_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let lower_name = file_name.to_ascii_lowercase();

    if lower_name.starts_with("test_")
        || lower_name.starts_with("test.")
        || re_test_sep().is_match(&lower_name)
        || re_test_camel().is_match(file_name)
    {
        return true;
    }

    if lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/__tests__/")
        || lower.contains("/spec/")
        || lower.contains("/specs/")
        || lower.contains("/testlib/")
        || lower.contains("/testing/")
        || lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.starts_with("spec/")
        || lower.starts_with("specs/")
        || re_test_dir().is_match(file_path)
    {
        return true;
    }

    matches_non_production_dir(&lower)
}

fn matches_non_production_dir(lower_path: &str) -> bool {
    const DIRS: &[&str] = &[
        "integration", "sample", "samples", "example", "examples", "fixture", "fixtures",
        "benchmark", "benchmarks", "demo", "demos",
    ];
    for d in DIRS {
        if lower_path.contains(&format!("/{}/", d)) || lower_path.starts_with(&format!("{}/", d)) {
            return true;
        }
    }
    false
}

fn re_test_sep() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"[._-](test|tests|spec|specs)\.[a-z0-9]+$").unwrap())
}
fn re_test_camel() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?:Test|Tests|TestCase|Tester|Spec|Specs)\.[A-Za-z0-9]+$").unwrap())
}
fn re_test_dir() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?:^|/)[A-Za-z0-9]*(?:Test|Tests|Spec)/").unwrap())
}

/// Whether `file_path` looks tool-generated (path-only). Port of `isGeneratedFile`.
pub fn is_generated_file(file_path: &str) -> bool {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    let re = R.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
            \.pb\.go$ | \.pulsar\.go$ | _grpc\.pb\.go$ |
            _mock\.go$ | _mocks\.go$ | (?:^|/)mock_[^/]+\.go$ |
            \.generated\.[jt]sx?$ | \.gen\.[jt]sx?$ | \.pb\.[jt]s$ | _pb\.[jt]s$ | _grpc_pb\.[jt]s$ |
            \.min\.m?js$ |
            _pb2(_grpc)?\.py$ | _pb2\.pyi$ |
            \.pb\.(cc|h)$ |
            \.g\.cs$ | Grpc\.cs$ |
            OuterClass\.java$ | Grpc\.java$ |
            \.pb\.swift$ |
            \.g\.dart$ | \.freezed\.dart$ | \.pb\.dart$ | \.pbgrpc\.dart$ | \.chopper\.dart$ |
            \.generated\.rs$
        ",
        )
        .unwrap()
    });
    re.is_match(file_path)
}

/// Whether a file is "low value" for the dominant-file heuristic: a test or a
/// generated file. Port of `isLowValueFile` (db/queries.ts).
pub fn is_low_value_file(file_path: &str) -> bool {
    let lp = file_path.to_ascii_lowercase();
    static R: OnceLock<regex::Regex> = OnceLock::new();
    let re = R.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
            (?:^|/)(tests?|__tests?__|spec)/ |
            _test\.go$ |
            (?:^|/)test_[^/]+\.py$ | _test\.py$ |
            _spec\.rb$ | _test\.rb$ |
            \.(test|spec)\.[jt]sx?$ |
            (test|spec|tests)\.(java|kt|scala)$ |
            (tests?|spec)\.cs$ |
            tests?\.swift$ |
            _test\.dart$
        ",
        )
        .unwrap()
    });
    re.is_match(&lp) || is_generated_file(file_path)
}

/// Bonus when a node's name matches the query. Port of `nameMatchBonus`.
pub fn name_match_bonus(node_name: &str, query: &str) -> i64 {
    let name_lower = node_name.to_ascii_lowercase();
    let raw_terms: Vec<String> = camel_space(query)
        .split(|c: char| c.is_whitespace() || "_.-".contains(c))
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() >= 2)
        .collect();
    let query_tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() >= 2)
        .collect();
    let query_lower: String = query.split_whitespace().collect::<String>().to_ascii_lowercase();

    if name_lower == query_lower {
        return 80;
    }
    if query_tokens.len() > 1 && query_tokens.iter().any(|t| *t == name_lower) {
        return 60;
    }
    if name_lower.starts_with(&query_lower) && !query_lower.is_empty() {
        let ratio = query_lower.chars().count() as f64 / name_lower.chars().count().max(1) as f64;
        return (10.0 + 30.0 * ratio).round() as i64;
    }
    if raw_terms.len() > 1 && raw_terms.iter().all(|t| name_lower.contains(t.as_str())) {
        return 15;
    }
    if !query_lower.is_empty() && name_lower.contains(&query_lower) {
        return 10;
    }
    0
}

/// Kind-based ranking bonus. Port of `kindBonus`.
pub fn kind_bonus(kind: NodeKind) -> i64 {
    use NodeKind::*;
    match kind {
        Function => 10,
        Method => 10,
        Class => 8,
        Interface => 9,
        TypeAlias => 6,
        Struct => 6,
        Trait => 9,
        Enum => 5,
        Component => 8,
        Route => 9,
        Module => 4,
        Property => 3,
        Field => 3,
        Variable => 2,
        Constant => 3,
        Import => 1,
        Export => 1,
        Parameter => 0,
        Namespace => 4,
        File => 0,
        Protocol => 9,
        EnumMember => 3,
    }
}

/// Whether a query token looks like a deliberate code identifier (camelCase /
/// snake_case / has digit / internal cap) vs a plain dictionary word. Port of
/// `isDistinctiveIdentifier`.
pub fn is_distinctive_identifier(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token.chars().any(|c| c == '_' || c.is_ascii_digit()) {
        return true;
    }
    token.chars().skip(1).any(|c| c.is_ascii_uppercase())
}

/// Derive project-name tokens (go.mod module / package.json name / repo dir).
/// Port of `deriveProjectNameTokens`. Returns normalized tokens ≥5 chars.
pub fn derive_project_name_tokens(project_root: &Path) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let add = |raw: &str, tokens: &mut HashSet<String>| {
        let norm = normalize_name_token(raw);
        if norm.len() >= 5 {
            tokens.insert(norm);
        }
    };

    if let Ok(gomod) = std::fs::read_to_string(project_root.join("go.mod")) {
        static RE: OnceLock<regex::Regex> = OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"(?m)^\s*module\s+(\S+)").unwrap());
        if let Some(c) = re.captures(&gomod) {
            if let Some(m) = c.get(1) {
                if let Some(last) = m.as_str().rsplit('/').next() {
                    add(last, &mut tokens);
                }
            }
        }
    }
    if let Ok(pkg) = std::fs::read_to_string(project_root.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
            if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                let stripped = name.rsplit('/').next().unwrap_or(name);
                add(stripped, &mut tokens);
            }
        }
    }
    if let Some(base) = project_root
        .canonicalize()
        .ok()
        .as_deref()
        .unwrap_or(project_root)
        .file_name()
        .and_then(|s| s.to_str())
    {
        add(base, &mut tokens);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_split_terms() {
        let terms = extract_search_terms("getUserById", false);
        assert!(terms.contains(&"get".to_string()));
        assert!(terms.contains(&"user".to_string()));
        assert!(terms.contains(&"getuserbyid".to_string()));
    }

    #[test]
    fn stems() {
        let v = get_stem_variants("caching");
        assert!(v.contains(&"cache".to_string()) || v.contains(&"cach".to_string()));
    }

    #[test]
    fn test_file_detection() {
        assert!(is_test_file("pkg/scheduler/schedule_one_test.go"));
        assert!(!is_test_file("pkg/scheduler/schedule_one.go"));
    }

    #[test]
    fn distinctive() {
        assert!(is_distinctive_identifier("getUser"));
        assert!(is_distinctive_identifier("MAX_RETRIES"));
        assert!(!is_distinctive_identifier("flat"));
    }
}
