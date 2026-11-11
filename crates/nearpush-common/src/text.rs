//! Text helpers and the turn-end rules verdict, shared by `nearpush` (the Kiro hook
//! CLI) and `nearpushd` (the daemon, which applies the same rules to relay events).
//! Moved out of the hook unchanged; the hook keeps its tests.

/// Longest question line or tool detail, in characters.
pub const MAX_DETAIL_CHARS: usize = 80;

pub fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

pub fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

pub fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if m % 60 == 0 { format!("{h}h") } else { format!("{h}h {}m", m % 60) }
}

fn looks_secret(word: &str) -> bool {
    let w = word.trim_matches(|c| matches!(c, '"' | '\'' | '`' | '(' | ')' | ',' | ';'));
    const PREFIXES: &[&str] =
        &["ghp_", "gho_", "ghs_", "ghu_", "github_pat_", "sk-", "xox", "AKIA", "ASIA", "eyJ", "tskey-", "glpat-", "npm_"];
    if PREFIXES.iter().any(|p| w.starts_with(p)) && w.len() > 8 {
        return true;
    }
    // Long opaque strings: tokens, hashes, base64 blobs.
    w.len() >= 24
        && w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
        && w.chars().any(|c| c.is_ascii_digit())
        && w.chars().any(|c| c.is_ascii_alphabetic())
        && !w.contains('/')
}

/// Hides env assignments (`KEY=value` → `KEY=…`), values after secret-looking
/// flags, `Bearer` credentials, URL userinfo and token-shaped words.
pub fn redact(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut hide_next = false;
    for word in text.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if hide_next {
            // `Authorization: Bearer <token>`: keep the scheme, hide what follows it.
            if matches!(lower.as_str(), "bearer" | "basic" | "token") {
                out.push(word.to_string());
            } else {
                out.push("…".into());
                hide_next = false;
            }
            continue;
        }
        let flag = lower.trim_start_matches('-');
        let secret_flag = word.starts_with('-')
            && ["token", "secret", "password", "passwd", "key", "auth", "credential"].iter().any(|k| flag.contains(k));
        if let Some((name, _)) = word.split_once('=') {
            let env_like = !name.is_empty()
                && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if env_like || secret_flag {
                out.push(format!("{name}=…"));
                continue;
            }
        }
        if secret_flag || lower == "bearer" || lower == "authorization:" {
            hide_next = true;
            out.push(word.to_string());
            continue;
        }
        if let Some(rest) = word.find("://").map(|i| &word[i + 3..]) {
            if let Some(at) = rest.find('@').filter(|&at| !rest[..at].contains('/')) {
                let scheme_end = word.len() - rest.len();
                out.push(format!("{}…@{}", &word[..scheme_end], &rest[at + 1..]));
                continue;
            }
        }
        if looks_secret(word) {
            out.push("…".into());
            continue;
        }
        out.push(word.to_string());
    }
    out.join(" ")
}

// ---- turn-end classifier ----------------------------------------------------------

/// Lines of the reply's end that the classifier looks at.
const CLASSIFY_TAIL_LINES: usize = 8;

/// Characters of the redacted reply tail sent to the daemon; the end is kept.
pub const CLASSIFIER_TAIL_CHARS: usize = 2000;

/// Lines at the end of the reply that the asking phrases are matched against.
const ASK_TAIL_LINES: usize = 2;

/// Phrases that ask the user for a decision (matched case-insensitively in the
/// last `ASK_TAIL_LINES` lines, quoted text excluded).
const ASK_PHRASES: &[&str] = &[
    "請選", "要不要", "你決定", "跟我說", "告訴我", "是否要", "可以嗎", "好嗎", "選哪", "哪一個",
    "which option", "should i", "let me know", "your call", "do you want", "would you like", "shall i",
    "want me to", "please confirm",
];

/// Words that make a `:`-ended intro line a choice prompt.
const CHOICE_CUES: &[&str] = &["option", "choose", "pick", "which", "選", "方案", "哪"];

/// Intro words that ask the user to pick now. They keep a choice list your_turn even
/// when an option is a future-tense plan ("Choose one: … 2. Ship later").
const DIRECT_CHOICE_CUES: &[&str] = &["choose", "pick", "which", "請選", "選一", "選哪", "哪"];

/// Strips markdown decoration: bold markers, headings, quotes, bullets, and
/// trailing `*` / `_`. Backticks stay, so `strip_quoted` can drop code spans.
fn strip_markdown(line: &str) -> String {
    let s = line.replace("**", "");
    let mut s = s.trim();
    loop {
        let before = s;
        s = s.trim_start_matches(['#', '>']).trim_start();
        for bullet in ["- ", "* ", "+ "] {
            if let Some(rest) = s.strip_prefix(bullet) {
                s = rest.trim_start();
            }
        }
        if s == before {
            break;
        }
    }
    s.trim_end_matches(['*', '_']).trim_end().to_string()
}

/// Removes quoted and code spans from one line, keeping the text outside them.
/// Used only for matching: a phrase that is quoted or mentioned is not a question
/// to the user. Pairs: 「」 and 『』 (nesting counted), “”, ‘’, "", and backticks.
/// ASCII `'…'` counts as a quote only when the opener does not follow a letter or
/// digit and the closer does not precede one, so `don't` and `I'm` stay plain.
/// An unclosed opener is plain text.
pub fn strip_quoted(line: &str) -> String {
    fn alnum(c: Option<&char>) -> bool {
        c.is_some_and(|c| c.is_alphanumeric())
    }
    fn simple_end(chars: &[char], i: usize, close: char) -> Option<usize> {
        chars[i + 1..].iter().position(|&c| c == close).map(|j| i + 2 + j)
    }
    fn nested_end(chars: &[char], i: usize, open: char, close: char) -> Option<usize> {
        let mut depth = 0usize;
        for (j, &c) in chars.iter().enumerate().skip(i) {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
        }
        None
    }
    fn span_end(chars: &[char], i: usize) -> Option<usize> {
        match chars[i] {
            '「' => nested_end(chars, i, '「', '」'),
            '『' => nested_end(chars, i, '『', '』'),
            '“' => simple_end(chars, i, '”'),
            '‘' => simple_end(chars, i, '’'),
            '"' => simple_end(chars, i, '"'),
            '`' => simple_end(chars, i, '`'),
            '\'' if i > 0 && alnum(chars.get(i - 1)) => None,
            '\'' => chars
                .iter()
                .enumerate()
                .skip(i + 1)
                .find(|&(j, &c)| c == '\'' && !alnum(chars.get(j + 1)))
                .map(|(j, _)| j + 1),
            _ => None,
        }
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        match span_end(&chars, i) {
            Some(end) => i = end,
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

fn ends_with_question(line: &str) -> bool {
    line.trim_end_matches([')', '）', '"', '」', '*', '_', ' ']).ends_with(['?', '？'])
}

/// `1.` `2)` `3、` `A.` `b)` `①` followed by text.
fn is_option_line(line: &str) -> bool {
    let s = line.trim_start();
    let mut chars = s.chars();
    let Some(c) = chars.next() else { return false };
    let rest: &str = if c.is_ascii_digit() {
        let r = s.trim_start_matches(|c: char| c.is_ascii_digit());
        match r.chars().next() {
            Some('.' | ')' | '、') => &r[r.chars().next().unwrap().len_utf8()..],
            _ => return false,
        }
    } else if matches!(c, 'A'..='D' | 'a'..='d') {
        match chars.next() {
            Some('.' | ')') => &s[2..],
            _ => return false,
        }
    } else if ('①'..='⑨').contains(&c) {
        &s[c.len_utf8()..]
    } else {
        return false;
    };
    rest.trim_start().chars().next().is_some_and(|c| !c.is_whitespace())
}

/// Did the turn end waiting on the user? Returns the question line (redacted,
/// truncated) for `your_turn`, `None` for `all_set`. Local and fast; never waits
/// on a model.
///
/// Rules, on the last 8 non-empty lines outside fenced code blocks. Matching uses
/// each line with quoted and code spans removed, so a quoted or mentioned phrase
/// is not a question. The returned detail is the line as written.
/// 1. The last line ends with `?` / `？`.
/// 2. At least two option lines (`1.`, `A)`, `①`…) ending within the last 2 lines,
///    in a group whose intro line, within 2 lines before it, ends with `?`, or ends
///    with `:` and names a choice ("option", "which", "選", "方案", "哪"…). A plain
///    "Changes:\n1. …" stays all_set. A `:` intro does not count when the group
///    holds a future-tense plan step (see rule 3) and the intro asks nothing itself.
/// 3. One of the last 2 lines contains an asking phrase ("should I", "let me know",
///    "要不要", "可以嗎"…). A phrase after a conditional marker on its line ("如果",
///    "若", "要是", "if") belongs to an offer the assistant acts on by default, so it
///    does not count. A phrase after a future-tense marker ("之後", "全部完成後",
///    "會再", "once everything is done") is part of a plan, so it does not count either,
///    unless a clause break (`，` `。` `;` `.` `?`…) separates the marker from it.
pub fn classify_turn_end(text: &str) -> Option<String> {
    // (as written, quoted and code spans removed)
    let mut lines: Vec<(String, String)> = Vec::new();
    let mut in_fence = false;
    for raw in text.lines() {
        let t = raw.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let l = strip_markdown(t);
        if !l.is_empty() {
            let matchable = strip_quoted(&l);
            lines.push((l, matchable));
        }
    }
    let tail = &lines[lines.len().saturating_sub(CLASSIFY_TAIL_LINES)..];
    let found = (|| {
        let (last_text, last) = tail.last()?;
        if ends_with_question(last) {
            return Some(last_text.clone());
        }
        // The option group must end within the last ASK_TAIL_LINES lines. Option
        // markers are read from the line as written, so `1. \`code\`` still counts.
        let window_start = tail.len().saturating_sub(ASK_TAIL_LINES);
        if let Some(end) = (window_start..tail.len()).rev().find(|&i| is_option_line(&tail[i].0)) {
            let mut start = end;
            while start > 0 && is_option_line(&tail[start - 1].0) {
                start -= 1;
            }
            if end - start + 1 >= 2 {
                // A list of future-tense plan steps is a status report, not a choice.
                let plan_list = tail[start..=end].iter().any(|(_, m)| FUTURE_MARKERS.iter().any(|f| contains_phrase(&m.to_lowercase(), f)));
                for (intro_text, intro) in tail[start.saturating_sub(2)..start].iter().rev() {
                    let lower = intro.to_lowercase();
                    let colon = intro.ends_with([':', '：']);
                    let direct = DIRECT_CHOICE_CUES.iter().any(|c| lower.contains(c));
                    let choice = colon && CHOICE_CUES.iter().any(|c| lower.contains(c)) && (!plan_list || direct || ask_phrase_counts(&lower));
                    if ends_with_question(intro) || choice {
                        return Some(intro_text.clone());
                    }
                }
            }
        }
        tail[tail.len().saturating_sub(ASK_TAIL_LINES)..]
            .iter()
            .rev()
            .find(|(_, l)| ask_phrase_counts(&l.to_lowercase()))
            .map(|(text, _)| text.clone())
    })()?;
    Some(truncate(&redact(&found), MAX_DETAIL_CHARS))
}

/// Conditional markers of an offer ("如果你想…我再…", "if you want…").
const CONDITIONAL_MARKERS: &[&str] = &["如果", "若", "要是", "if"];

/// Markers of a future-tense plan or warning ("全部換完後會再停", "once everything is
/// done I will need"). Asking phrases after one of them are part of the plan.
const FUTURE_MARKERS: &[&str] = &[
    "之後", "稍後", "到時候", "完成後", "換完後", "做完後", "會再", "會跟你", "會問你", "會請你", "once everything",
    "once done", "once it's done", "after everything", "when everything", "after that", "once that", "after this",
    "after it is", "after it's", "later",
];

/// Clause separators. A future marker covers only the asking phrases in its own clause,
/// so a question after a comma or full stop is judged on its own.
const CLAUSE_BREAKS: &[char] = &['，', '。', '；', ';', ',', '.', '！', '!', '？', '?'];

/// Whether the asking phrases of `lower` (a lowercased line, quoted spans removed)
/// count. A phrase after a conditional marker on the same line is part of an offer, so
/// it does not count. A phrase after a future-tense marker is part of a plan only when
/// the marker sits in the same clause. "全部完成後會再停一次，要不要現在就 ship？" keeps
/// its question. "Let me know if you want more." still counts: its phrase comes before
/// the marker.
fn ask_phrase_counts(lower: &str) -> bool {
    let first_conditional = CONDITIONAL_MARKERS.iter().filter_map(|m| phrase_starts(lower, m).next()).min();
    let future: Vec<usize> = FUTURE_MARKERS.iter().flat_map(|m| phrase_starts(lower, m)).collect();
    ASK_PHRASES.iter().any(|p| {
        phrase_starts(lower, p).any(|at| {
            first_conditional.is_none_or(|marker| at < marker)
                && !future.iter().any(|&m| m < at && !lower[m..at].contains(CLAUSE_BREAKS))
        })
    })
}

/// Substring match; ASCII phrases must also sit on word boundaries, so
/// "should i" does not match "should include".
fn contains_phrase(hay: &str, phrase: &str) -> bool {
    phrase_starts(hay, phrase).next().is_some()
}

/// Start offsets of `phrase` in `hay`, skipping ASCII matches inside a word.
fn phrase_starts<'a>(hay: &'a str, phrase: &'a str) -> impl Iterator<Item = usize> + 'a {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() && c.is_ascii());
    let ascii = phrase.is_ascii();
    hay.match_indices(phrase).filter_map(move |(i, m)| {
        let bounded = !ascii || (!word(hay[..i].chars().next_back()) && !word(hay[i + m.len()..].chars().next()));
        bounded.then_some(i)
    })
}

/// The reply's last non-empty lines outside code fences, redacted, for the daemon's
/// classifier. Keeps the last `CLASSIFY_TAIL_LINES` lines and at most
/// `CLASSIFIER_TAIL_CHARS` characters (the end is kept). `None` when nothing is left.
pub fn classifier_tail(text: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for raw in text.lines() {
        let t = raw.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && !t.is_empty() {
            lines.push(t);
        }
    }
    let tail: Vec<String> = lines[lines.len().saturating_sub(CLASSIFY_TAIL_LINES)..].iter().map(|l| redact(l)).collect();
    let joined = tail.join("\n");
    let n = joined.chars().count();
    let kept: String = joined.chars().skip(n.saturating_sub(CLASSIFIER_TAIL_CHARS)).collect();
    (!kept.trim().is_empty()).then_some(kept)
}
