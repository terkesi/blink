//! Per-candidate relation context for the shared-identifier and evidence re-asks.
//!
//! A re-asked window arrives with two things drawn from source that was already read: the
//! declaration lines that enclose it (the `impl`, `class` or function header above its first
//! line) and one verified relationship excerpt: a few lines around a call or use of a name the
//! window declares, or, failing that, around the declaration of a name it calls, or, for a
//! shared-identifier batch, the donor lines that carry the shared identifier. Everything comes
//! from the prepared snapshot; nothing here reads a file or sends a request, and the context
//! shrinks (excerpt first, then header) until the request fits the pass's byte allowance.
use super::*;
use std::{collections::HashMap, fmt::Write};

/// Name of the `related_source` slot when it carries relation context for a shared-identifier
/// batch. Evidence batches keep their `evidence` slot and put the relation context before the card.
pub(super) const SLOT_NAME: &str = "relations";
/// Enclosing declaration lines kept per target, innermost first when the header must shrink.
const HEADER_LINES: usize = 3;
/// Longest rendered context line; longer source lines are cut.
const LINE_CHARS: usize = 160;
/// Lines kept around a relationship (two before and three after a call; the declaration and
/// the five lines after it).
const EXCERPT_LINES: usize = 6;
/// Fitting stages as (excerpt lines, header lines): the excerpt shrinks and disappears before
/// the header gives up its outer lines. When the last stage does not fit, the pass keeps its
/// previous context.
const STAGES: [(usize, usize); 6] = [(6, 3), (4, 3), (2, 3), (1, 3), (0, 3), (0, 1)];
const CONTROL: &str = "if else elif for while match switch case do try catch finally with loop \
     return when unless until select defer go";
/// A name declared by more prepared windows than this (`Ok`, `new`, `fmt`) names no verifiable
/// relationship; such names never pick the excerpt.
const AMBIGUOUS: usize = 8;
/// Shortest name part and query term that count as naming the queried behavior.
const TERM_CHARS: usize = 3;

/// Preference among relationship candidates: accepted, judged, then unjudged same-file windows;
/// a name that shares a part with the query's words; a call before a bare use; the rarest name
/// (fewest declaring windows); the strongest judgment; the nearest window; the lowest index.
type Key = (u8, u8, u8, usize, u64, usize, usize);
/// A chosen relationship: its key, the window holding it, the related name and the line offset
/// of the call, use or declaration within that window.
type Choice<'a> = (Key, usize, &'a str, usize);

/// What the planner already knows about judged windows when the context is built.
pub(super) struct Judged<'a> {
    pub probabilities: &'a BTreeMap<usize, Option<f64>>,
    pub fresh: &'a BTreeSet<usize>,
    pub threshold: f64,
}

impl Judged<'_> {
    fn accepted(&self, prepared: &Prepared, index: usize) -> bool {
        self.probabilities
            .get(&index)
            .copied()
            .flatten()
            .is_some_and(|p| p >= self.threshold)
            && self.fresh.contains(&prepared.windows[index].file)
    }

    /// Accepted windows first, then judged ones, then unjudged windows of the target's own file.
    fn class(&self, prepared: &Prepared, index: usize) -> u8 {
        if self.accepted(prepared, index) {
            0
        } else if self.probabilities.contains_key(&index) {
            1
        } else {
            2
        }
    }

    fn rank(&self, index: usize) -> u64 {
        let probability = self.probabilities.get(&index).copied().flatten();
        ((1.0 - probability.unwrap_or(0.0)) * 1_000_000.0) as u64
    }
}

pub(super) struct Relation {
    name: String,
    path: String,
    header: Vec<(usize, String)>,
    excerpt: Option<Excerpt>,
}

struct Excerpt {
    label: String,
    lines: Vec<(usize, String)>,
    center: usize,
    forward: bool,
}

impl Excerpt {
    fn span(&self, count: usize) -> (usize, usize) {
        let last = self.lines.len() - 1;
        let first = if self.forward {
            self.center
        } else {
            self.center.saturating_sub((count - 1) / 2)
        };
        (first, (first + count - 1).min(last))
    }
}

impl Relation {
    fn is_empty(&self) -> bool {
        self.header.is_empty() && self.excerpt.is_none()
    }

    /// The context at its fullest stage, for tests.
    #[cfg(test)]
    pub(super) fn text(&self) -> String {
        let mut out = String::new();
        self.render(&mut out, EXCERPT_LINES, HEADER_LINES);
        out
    }

    fn render(&self, out: &mut String, excerpt_lines: usize, header_lines: usize) {
        if !self.header.is_empty() {
            let kept = &self.header[self.header.len().saturating_sub(header_lines)..];
            let numbers: Vec<String> = kept.iter().map(|(number, _)| number.to_string()).collect();
            let _ = writeln!(
                out,
                "// {} enclosing declarations, {}:{}",
                self.name,
                self.path,
                numbers.join(",")
            );
            for (_, line) in kept {
                out.push_str(line);
                out.push('\n');
            }
        }
        if excerpt_lines > 0
            && let Some(excerpt) = &self.excerpt
        {
            let (first, last) = excerpt.span(excerpt_lines);
            let _ = writeln!(
                out,
                "// {} {}:{}-{}",
                self.name, excerpt.label, excerpt.lines[first].0, excerpt.lines[last].0
            );
            for (_, line) in &excerpt.lines[first..=last] {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
}

/// Declarations of every prepared window, built once per planning call, the query's words, and
/// a lazy cache of the identifiers each consulted window mentions.
pub(super) struct Index<'a> {
    prepared: &'a Prepared,
    terms: BTreeSet<String>,
    declared: Vec<BTreeSet<&'a str>>,
    declaring: BTreeMap<&'a str, Vec<usize>>,
    words: HashMap<usize, BTreeSet<&'a str>>,
}

fn window_text(prepared: &Prepared, index: usize) -> &str {
    let window = &prepared.windows[index];
    &prepared.snapshot.files()[window.file].text()[window.start..window.end]
}

fn words(text: &str) -> BTreeSet<&str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .collect()
}

/// An upper-case member or constant assigned without a keyword (`DEFAULT = enum.auto()`), which
/// the callee pass's keyword patterns do not count as a declaration but comparisons elsewhere
/// refer to.
fn constant(line: &str) -> Option<&str> {
    let (name, rest) = callees::word(line.trim_start());
    let rest = rest.trim_start();
    (name.chars().count() >= 2
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && rest.starts_with('=')
        && !rest.starts_with("=="))
    .then_some(name)
}

fn declares(line: &str) -> Option<&str> {
    callees::declaration(line).or_else(|| constant(line))
}

fn declarations(text: &str) -> BTreeSet<&str> {
    text.lines().filter_map(declares).collect()
}

/// Whether a code line refers to `name` outside its own declaration, and whether that reference
/// is a call. A call is the name followed by `(`; a use is the name reached through `.`, `::`
/// or `->`, or a bare capitalised name (a type, class or constant). A bare lower-case word is a
/// variable that happens to share the name, not a reference.
fn mention(line: &str, name: &str) -> Option<bool> {
    let trimmed = line.trim_start();
    if is_comment(trimmed) || declares(line) == Some(name) {
        return None;
    }
    let mut call = None;
    for (offset, _) in line.match_indices(name) {
        let before = &line[..offset];
        let after = &line[offset + name.len()..];
        if before.ends_with(|c: char| c.is_alphanumeric() || c == '_')
            || after.starts_with(|c: char| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        if after.trim_start_matches([' ', '\t']).starts_with('(') {
            return Some(true);
        }
        if before.ends_with(['.', ':', '>']) || name.starts_with(|c: char| c.is_uppercase()) {
            call = Some(false);
        }
    }
    call
}

fn indentation(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

fn cut(line: &str) -> String {
    let line = line.trim_end();
    match line.char_indices().nth(LINE_CHARS) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line.to_owned(),
    }
}

fn is_comment(trimmed: &str) -> bool {
    ["//", "#", "/*", "*", "--", "<!--"]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn is_continuation(trimmed: &str) -> bool {
    trimmed.starts_with([')', ']', '.'])
}

fn has_call(line: &str) -> bool {
    line.match_indices('(')
        .any(|(offset, _)| line[..offset].ends_with(|c: char| c.is_alphanumeric() || c == '_'))
}

/// A declaration per the callee pass, a Rust `impl` or `macro_rules!` header, or a braced
/// signature without a keyword (JavaScript and TypeScript methods, C and Java functions).
fn is_header(trimmed: &str) -> bool {
    if callees::declaration(trimmed).is_some() {
        return true;
    }
    let mut rest = trimmed;
    loop {
        let (first, tail) = callees::word(rest);
        if callees::is_modifier(first) {
            rest = callees::skip_group(tail);
            continue;
        }
        if first == "impl" || (first == "macro_rules" && tail.starts_with('!')) {
            return true;
        }
        return first.starts_with(|c: char| c.is_alphabetic() || c == '_')
            && !CONTROL.split(' ').any(|keyword| keyword == first)
            && trimmed.ends_with('{')
            && !trimmed.contains("=>")
            && has_call(trimmed);
    }
}

/// The declaration lines that enclose the window's first non-blank line, outermost first:
/// walking upwards, a header at a smaller indentation than everything seen so far encloses the
/// window; any other line at a smaller indentation (a closing brace, an assignment, a
/// decorator or annotation) closes the scopes above it, except continuation lines of a
/// multi-line signature and comments.
fn enclosing(prepared: &Prepared, target: usize) -> Vec<(usize, String)> {
    let window = &prepared.windows[target];
    let file = &prepared.snapshot.files()[window.file];
    let text = file.text();
    let offsets = file.line_offsets();
    let line = |number: usize| -> &str {
        let start = offsets[number - 1];
        let end = offsets.get(number).copied().unwrap_or(text.len());
        text[start..end].trim_end_matches(['\n', '\r'])
    };
    let Some(first) = (window.start_line..=window.end_line.min(offsets.len()))
        .find(|&n| !line(n).trim().is_empty())
    else {
        return Vec::new();
    };
    let mut limit = indentation(line(first));
    let mut found = Vec::new();
    for number in (1..first).rev() {
        if limit == 0 || found.len() == HEADER_LINES {
            break;
        }
        let candidate = line(number);
        let trimmed = candidate.trim_start();
        if trimmed.is_empty() || is_comment(trimmed) {
            continue;
        }
        let indent = indentation(candidate);
        if indent >= limit {
            continue;
        }
        if is_header(trimmed) {
            found.push((number, cut(candidate)));
            limit = indent;
        } else if !is_continuation(trimmed) {
            limit = indent;
        }
    }
    found.reverse();
    found
}

impl<'a> Index<'a> {
    pub(super) fn new(
        prepared: &'a Prepared,
        query: &str,
        control: &mut dyn FnMut() -> Control,
    ) -> Option<Self> {
        let mut declared = Vec::with_capacity(prepared.windows.len());
        let mut declaring: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for index in 0..prepared.windows.len() {
            if control() != Control::Continue {
                return None;
            }
            let names = declarations(window_text(prepared, index));
            for &name in &names {
                declaring.entry(name).or_default().push(index);
            }
            declared.push(names);
        }
        Some(Self {
            prepared,
            terms: terms(query)
                .into_iter()
                .filter(|term| term.chars().count() >= TERM_CHARS)
                .collect(),
            declared,
            declaring,
            words: HashMap::new(),
        })
    }

    fn words(&mut self, index: usize) -> &BTreeSet<&'a str> {
        let prepared = self.prepared;
        self.words
            .entry(index)
            .or_insert_with(|| words(window_text(prepared, index)))
    }

    /// Names declared by too many windows name no verifiable relationship.
    fn ambiguous(&self, name: &str) -> bool {
        self.declaring
            .get(name)
            .is_some_and(|windows| windows.len() > AMBIGUOUS)
    }

    /// Whether a part of `name` (split at underscores and case changes) is one of the query's
    /// words, so the relationship concerns the queried behavior rather than a neighbour.
    fn queried(&self, name: &str) -> bool {
        terms(name)
            .iter()
            .any(|part| part.chars().count() >= TERM_CHARS && self.terms.contains(part))
    }

    /// Windows whose text may serve as the relationship excerpt: every window of the target's
    /// file and accepted windows elsewhere whose file is still fresh.
    fn eligible(&self, target: usize, judged: &Judged) -> Vec<usize> {
        let windows = &self.prepared.windows;
        let file = windows[target].file;
        let mut first = target;
        while first > 0 && windows[first - 1].file == file {
            first -= 1;
        }
        let mut last = target + 1;
        while last < windows.len() && windows[last].file == file {
            last += 1;
        }
        let mut eligible: Vec<usize> = (first..last).filter(|&index| index != target).collect();
        eligible.extend(
            judged.probabilities.keys().copied().filter(|&index| {
                windows[index].file != file && judged.accepted(self.prepared, index)
            }),
        );
        eligible
    }

    fn key(&self, judged: &Judged, target: usize, other: usize, name: &str, call: bool) -> Key {
        let windows = &self.prepared.windows;
        let distance = if windows[other].file == windows[target].file {
            windows[other]
                .start_line
                .saturating_sub(windows[target].end_line)
                .max(
                    windows[target]
                        .start_line
                        .saturating_sub(windows[other].end_line),
                )
        } else {
            usize::MAX
        };
        (
            judged.class(self.prepared, other),
            u8::from(!self.queried(name)),
            u8::from(!call),
            self.declaring.get(name).map_or(0, Vec::len),
            judged.rank(other),
            distance,
            other,
        )
    }

    /// The first line of `other` outside the target's own lines that satisfies `matches`.
    fn line_in(
        &self,
        target: usize,
        other: usize,
        matches: impl Fn(&str) -> bool,
    ) -> Option<usize> {
        let windows = &self.prepared.windows;
        let same_file = windows[other].file == windows[target].file;
        window_text(self.prepared, other)
            .lines()
            .enumerate()
            .position(|(offset, line)| {
                let number = windows[other].start_line + offset;
                !(same_file
                    && (windows[target].start_line..=windows[target].end_line).contains(&number))
                    && matches(line)
            })
    }

    fn excerpt(
        &self,
        judged: &Judged,
        other: usize,
        center: usize,
        forward: bool,
        relation: &str,
    ) -> Excerpt {
        let window = &self.prepared.windows[other];
        let path = self.prepared.snapshot.files()[window.file].path();
        let accepted = if judged.accepted(self.prepared, other) {
            "accepted "
        } else {
            ""
        };
        let first = if forward {
            center
        } else {
            center.saturating_sub((EXCERPT_LINES - 1) / 2)
        };
        let lines: Vec<(usize, String)> = window_text(self.prepared, other)
            .lines()
            .enumerate()
            .skip(first)
            .take(EXCERPT_LINES)
            .map(|(offset, line)| (window.start_line + offset, cut(line)))
            .collect();
        Excerpt {
            label: format!("{relation} {accepted}{path}"),
            lines,
            center: center - first,
            forward,
        }
    }

    /// A window that calls or uses a name the target declares. A use in another file counts
    /// only when that file does not declare the name itself.
    fn caller(&mut self, target: usize, judged: &Judged) -> Option<Excerpt> {
        let declared: Vec<&'a str> = self.declared[target]
            .iter()
            .copied()
            .filter(|name| !self.ambiguous(name))
            .collect();
        if declared.is_empty() {
            return None;
        }
        let windows = &self.prepared.windows;
        let mut best: Option<(Choice<'a>, bool)> = None;
        for other in self.eligible(target, judged) {
            let hits: Vec<&'a str> = {
                let words = self.words(other);
                declared
                    .iter()
                    .copied()
                    .filter(|name| words.contains(name))
                    .collect()
            };
            for name in hits {
                let foreign = windows[other].file != windows[target].file;
                if foreign
                    && self.declaring[name]
                        .iter()
                        .any(|&index| windows[index].file == windows[other].file)
                {
                    continue;
                }
                for call in [true, false] {
                    let key = self.key(judged, target, other, name, call);
                    if best
                        .as_ref()
                        .is_some_and(|((previous, ..), _)| *previous <= key)
                    {
                        continue;
                    }
                    if let Some(line) =
                        self.line_in(target, other, |line| mention(line, name) == Some(call))
                    {
                        best = Some(((key, other, name, line), call));
                        break;
                    }
                }
            }
        }
        best.map(|((_, other, name, line), call)| {
            let relation = if call { "called from" } else { "used in" };
            self.excerpt(
                judged,
                other,
                line,
                false,
                &format!("declares {name}, {relation}"),
            )
        })
    }

    /// The declaration of a name the target calls or uses, when no known window calls or uses a
    /// name the target declares. A declaration in the target's own file beats any other, and a
    /// declaration already shown as the target's enclosing header is not repeated.
    fn declaration(
        &mut self,
        target: usize,
        judged: &Judged,
        header: &[(usize, String)],
    ) -> Option<Excerpt> {
        let mentioned: Vec<&'a str> = self.words(target).iter().copied().collect();
        let lines: Vec<&str> = window_text(self.prepared, target).lines().collect();
        let mentioned: Vec<(&'a str, bool)> = mentioned
            .into_iter()
            .filter(|name| {
                self.declaring.contains_key(name)
                    && !self.declared[target].contains(name)
                    && !self.ambiguous(name)
            })
            .filter_map(|name| {
                // A bare lower-case use of a declared name is as likely a field as the
                // function or property declared elsewhere; only calls and type names bind.
                lines
                    .iter()
                    .filter_map(|line| mention(line, name))
                    .max()
                    .filter(|&call| call || name.starts_with(|c: char| c.is_uppercase()))
                    .map(|call| (name, call))
            })
            .collect();
        let windows = &self.prepared.windows;
        let mut best: Option<(Choice<'a>, bool)> = None;
        for (name, call) in mentioned {
            let declaring = &self.declaring[name];
            let local = declaring
                .iter()
                .any(|&index| index != target && windows[index].file == windows[target].file);
            for &other in declaring {
                if other == target {
                    continue;
                }
                let foreign = windows[other].file != windows[target].file;
                if foreign && (local || !judged.accepted(self.prepared, other)) {
                    continue;
                }
                let key = self.key(judged, target, other, name, call);
                if best
                    .as_ref()
                    .is_some_and(|((previous, ..), _)| *previous <= key)
                {
                    continue;
                }
                if let Some(line) = self.line_in(target, other, |line| declares(line) == Some(name))
                    && !(windows[other].file == windows[target].file
                        && header
                            .iter()
                            .any(|&(number, _)| number == windows[other].start_line + line))
                {
                    best = Some(((key, other, name, line), call));
                }
            }
        }
        best.map(|((_, other, name, line), call)| {
            let relation = if call { "calls" } else { "uses" };
            self.excerpt(
                judged,
                other,
                line,
                true,
                &format!("{relation} {name}, declared in"),
            )
        })
    }

    /// The donor lines that carry the longest identifier the target shares with it.
    fn shared(&self, target: usize, donor: usize, judged: &Judged) -> Option<Excerpt> {
        let target_words = related::identifiers(window_text(self.prepared, target));
        let donor_words = related::identifiers(window_text(self.prepared, donor));
        let word = target_words
            .intersection(&donor_words)
            .max_by_key(|word| (word.chars().count(), std::cmp::Reverse(*word)))?;
        let carries = |line: &str| words(line).contains(word);
        // A code line carrying the identifier beats a comment that merely mentions it.
        let line = self
            .line_in(target, donor, |line| {
                !is_comment(line.trim_start()) && carries(line)
            })
            .or_else(|| self.line_in(target, donor, carries))?;
        Some(self.excerpt(
            judged,
            donor,
            line,
            false,
            &format!("shares identifier {word} with"),
        ))
    }

    pub(super) fn relation(
        &mut self,
        target: usize,
        judged: &Judged,
        donor: Option<usize>,
    ) -> Relation {
        let header = enclosing(self.prepared, target);
        let excerpt = self
            .caller(target, judged)
            .or_else(|| self.declaration(target, judged, &header))
            .or_else(|| donor.and_then(|donor| self.shared(target, donor, judged)));
        let window = &self.prepared.windows[target];
        Relation {
            name: format!("w{target}"),
            path: self.prepared.snapshot.files()[window.file]
                .path()
                .to_owned(),
            header,
            excerpt,
        }
    }
}

/// Encodes `candidates` with their relation context in the `related_source` slot described by
/// `slot` (its text is replaced), followed by `tail` when an evidence card accompanies it. The
/// context shrinks through `STAGES` until the request grows by at most `max_extra` bytes over
/// the plain batch; `None` means no stage fit or no candidate had any context, and the caller
/// keeps its previous context.
pub(super) fn encode(
    query: &str,
    candidates: &[Candidate<'_>],
    relations: &[Relation],
    slot: Candidate<'_>,
    tail: Option<&str>,
    max_extra: usize,
) -> Option<Batch> {
    if relations.iter().all(Relation::is_empty) {
        return None;
    }
    for (excerpt_lines, header_lines) in STAGES {
        let mut text = String::new();
        for relation in relations {
            relation.render(&mut text, excerpt_lines, header_lines);
        }
        if text.is_empty() {
            return None;
        }
        match tail {
            Some(tail) => text.push_str(tail),
            None => {
                text.pop();
            }
        }
        let context = Candidate {
            text: &text,
            ..slot
        };
        if let Ok(Some(batch)) = Batch::encode_with_context(query, candidates, &context, max_extra)
        {
            return Some(batch);
        }
    }
    None
}

/// Re-encodes one refused target alone with its relation context, for the refusal retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn single(
    prepared: &Prepared,
    query: &str,
    target: usize,
    judged: &Judged,
    donor: Option<usize>,
    slot: Candidate<'_>,
    tail: Option<&str>,
    max_extra: usize,
    control: &mut dyn FnMut() -> Control,
) -> Option<Batch> {
    let mut index = Index::new(prepared, query, control)?;
    let relation = index.relation(target, judged, donor);
    let name = format!("w{target}");
    let candidate = candidate(prepared, target, &name);
    encode(
        query,
        std::slice::from_ref(&candidate),
        std::slice::from_ref(&relation),
        slot,
        tail,
        max_extra,
    )
}
