//! What happens to a transcript between the decoder and the clipboard.
//!
//! Everything here is deterministic string work: no model, no network, no second
//! download. That is deliberate. Every competitor's headline feature is a cleanup
//! pass, and the ones that do it well either send your words to a cloud model or
//! make you install a multi-gigabyte local one. Rules handle the common cases —
//! fillers, repeats, spoken punctuation, a personal dictionary — and a rule either
//! fires or does not, which is testable in a way "the model felt like it" is not.
//!
//! The order matters and is fixed: cleanup, then a whole-utterance command, then a
//! whole-utterance snippet, then punctuation and spoken formatting, then the
//! user's replacements. A snippet therefore never gets rewritten by a replacement,
//! and a command never fires in the middle of a sentence.

use crate::config::Config;

/// What one utterance should do instead of being pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Throw it away: "scratch that".
    Discard,
    /// Stop the daemon: "quit myware".
    Quit,
}

impl Command {
    pub fn arg(self) -> &'static str {
        match self {
            Command::Discard => "discard",
            Command::Quit => "quit",
        }
    }
}

/// Either text to deliver, or an instruction to follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Text(String),
    Command(Command),
}

/// A trailing instruction that reshapes the whole utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Join everything onto one line.
    OneLine,
    /// One bullet per sentence.
    Bullets,
}

/// Recognise and strip a trailing instruction: "… as a list", "… on one line".
///
/// Only the suffix is consumed; the words before it are the utterance. Kept
/// separate from [`apply`] so the transform happens after formatting, where a
/// newline it inserts is not collapsed again.
fn shape_of(text: &str, rules: &[crate::config::CommandRule]) -> (String, Option<Shape>) {
    let lower = text.to_lowercase();
    // The suffixes are ASCII in the defaults; a rule with other scripts falls back
    // to the original text rather than slicing mid-character.
    if lower.len() != text.len() {
        return (text.to_string(), None);
    }
    for rule in rules {
        for say in alternatives(&rule.say) {
            let suffix = format!(" {}", say.to_lowercase());
            if !lower.ends_with(&suffix) {
                continue;
            }
            let shape = match rule.action.trim().to_lowercase().as_str() {
                "bullets" => Shape::Bullets,
                "one line" => Shape::OneLine,
                _ => continue,
            };
            let cut = text.len() - suffix.len();
            return (text[..cut].trim_end().to_string(), Some(shape));
        }
    }
    (text.to_string(), None)
}

/// The phrases one rule matches: `say`, `from` or `trigger` may name several,
/// comma-separated.
///
/// "exclamation mark, exclamation point" is one rule with two ways to say it,
/// which is how the default tables are written. A comma is therefore a
/// separator and not a literal; the punctuation table spells the mark "comma"
/// for that reason, and the settings window says so.
fn alternatives(field: &str) -> impl Iterator<Item = &str> {
    field.split(',').map(str::trim).filter(|part| !part.is_empty())
}

/// One bullet per sentence, split after terminal punctuation.
fn bullets(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let split = rest
            .find(['.', '!', '?'])
            .map(|index| index + 1)
            .unwrap_or(rest.len());
        let (sentence, tail) = rest.split_at(split);
        let sentence = sentence.trim();
        if !sentence.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("\u{2022} ");
            out.push_str(sentence);
        }
        rest = tail.trim_start();
    }
    out
}

/// The whole pipeline for one utterance.
pub fn apply(raw: &str, config: &Config) -> Outcome {
    let cleaned = if config.cleanup {
        clean(raw, config)
    } else {
        raw.trim().to_string()
    };

    if config.commands {
        if let Some(command) = command_for(&cleaned, &config.command_rules) {
            return Outcome::Command(command);
        }
    }
    if let Some(snippet) = snippet_for(&cleaned, config) {
        return Outcome::Text(snippet.to_string());
    }

    // A trailing "as a list" is an instruction rather than words to keep, and it
    // travels with voice commands: both are things you say to the app.
    let (mut text, shape) = if config.commands {
        shape_of(&cleaned, &config.shape_rules)
    } else {
        (cleaned, None)
    };
    if config.punctuation_commands {
        text = punctuation(&text, &config.punctuation);
    }
    if config.formatting {
        text = formatting(&text, &config.formatting_rules);
    }
    text = replace(&text, config);
    // Last, so the newlines a shape inserts are not tidied away. A terminal still
    // gets one line: the daemon flattens by focus, which is where bullets become
    // pointless anyway.
    let text = match shape {
        Some(Shape::OneLine) => flatten(&text),
        Some(Shape::Bullets) => bullets(&text),
        None => text,
    };
    Outcome::Text(tidy(&text))
}

/// One line out: newlines become spaces.
///
/// A multi-line payload and a terminal are a command injection. "New paragraph"
/// exists for prose, but the same text pasted into a shell would run whatever
/// followed the newline, and the paste chord carries no notion of where the line
/// came from. This is the last place the distinction can be made.
pub fn flatten(text: &str) -> String {
    tidy(&text.replace('\n', " "))
}

/// The part that is safe mid-sentence, for streaming chunks.
///
/// A command or a snippet has to see the whole utterance: applying one to a chunk
/// would fire on the first phrase of a paragraph that goes on to say something
/// else.
pub fn cleanup_only(text: &str, config: &Config) -> String {
    if config.cleanup {
        clean(text, config)
    } else {
        text.to_string()
    }
}

fn clean(raw: &str, config: &Config) -> String {
    let mut text = raw.trim().to_string();
    for filler in &config.fillers {
        if !filler.trim().is_empty() {
            text = remove_word(&text, filler);
        }
    }
    text = collapse_repeats(&text);
    tidy(&text)
}

fn punctuation(text: &str, rules: &[crate::config::SpokenRule]) -> String {
    // Longest phrase first, so "question mark" wins over a rule for "mark". Each
    // comma-separated alternative is its own phrase: one rule can name several
    // ways to say the same mark.
    let mut phrases: Vec<(&str, &str)> = rules
        .iter()
        .flat_map(|rule| {
            alternatives(&rule.say).map(move |say| (say, rule.becomes.as_str()))
        })
        .collect();
    phrases.sort_by_key(|(say, _)| std::cmp::Reverse(say.len()));
    let mut out = text.to_string();
    for (say, becomes) in phrases {
        out = substitute(&out, say, becomes, true);
    }
    tidy(&out)
}

fn formatting(text: &str, rules: &[crate::config::SpokenRule]) -> String {
    let mut out = text.to_string();
    for rule in rules {
        for say in alternatives(&rule.say) {
            out = substitute(&out, say, &rule.becomes, true);
        }
    }
    tighten(&out, &['@'])
}

fn replace(text: &str, config: &Config) -> String {
    let mut out = text.to_string();
    for rule in &config.replacements {
        for from in alternatives(&rule.from) {
            out = substitute(&out, from, &rule.to, false);
        }
    }
    out
}

/// Exact-match commands, on the whole utterance.
fn command_for(text: &str, rules: &[crate::config::CommandRule]) -> Option<Command> {
    let said = normalise(text);
    for rule in rules {
        if !alternatives(&rule.say).any(|say| normalise(say) == said) {
            continue;
        }
        return match rule.action.trim().to_lowercase().as_str() {
            "discard" => Some(Command::Discard),
            "quit" => Some(Command::Quit),
            // An action this build does not know is not text to keep either.
            _ => None,
        };
    }
    None
}

/// A phrase that expands to fixed text, matched on the whole utterance.
fn snippet_for<'a>(text: &str, config: &'a Config) -> Option<&'a str> {
    let wanted = normalise(text);
    config
        .snippets
        .iter()
        .find(|snippet| {
            alternatives(&snippet.trigger).any(|trigger| normalise(trigger) == wanted)
        })
        .map(|snippet| snippet.text.as_str())
}

/// Case-fold, drop terminals and collapse spaces: what "the same phrase" means.
fn normalise(text: &str) -> String {
    text.trim()
        .trim_matches(|character: char| character.is_ascii_punctuation() && character != '\'')
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replace whole-word `needle` with `replacement`, case-insensitively.
///
/// `eat_space` swallows the single space on either side, so "hello comma world"
/// becomes "hello, world" rather than "hello , world".
fn substitute(text: &str, needle: &str, replacement: &str, eat_space: bool) -> String {
    let lower = text.to_lowercase();
    // `to_lowercase` can change byte length for a few scripts; the offsets below
    // are byte offsets, so bail out rather than slice a boundary wrongly.
    if lower.len() != text.len() || needle.is_empty() {
        return text.to_string();
    }
    let haystack = lower.as_bytes();
    let needle = needle.to_lowercase();
    let needle = needle.as_bytes();

    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        let matches = &haystack[index..index + needle.len()] == needle;
        let before = index.checked_sub(1).map(|i| haystack[i]).unwrap_or(b' ');
        let after = haystack.get(index + needle.len()).copied().unwrap_or(b' ');
        let whole = !word_byte(before) && !word_byte(after);
        if !matches || !whole {
            index += 1;
            continue;
        }
        let mut start = index;
        let mut end = index + needle.len();
        if eat_space {
            if start > 0 && haystack[start - 1] == b' ' {
                start -= 1;
            }
            if haystack.get(end).copied() == Some(b' ') {
                end += 1;
            }
        }
        out.push_str(&text[copied..start]);
        out.push_str(replacement);
        copied = end;
        index = end;
    }
    out.push_str(&text[copied..]);
    out
}

fn remove_word(text: &str, word: &str) -> String {
    substitute(text, word, "", true)
}

fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'\'' || byte == b'-' || byte >= 0x80
}

/// Drop an immediately repeated run of one to three words: "the the", "I think I
/// think". Compared case-insensitively, keeping the first run's spelling.
fn collapse_repeats(text: &str) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let same = |left: &[&str], right: &[&str]| {
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    };

    let mut out: Vec<&str> = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        let mut skipped = 0;
        for span in (1..=3).rev() {
            if index + 2 * span <= tokens.len()
                && same(
                    &tokens[index..index + span],
                    &tokens[index + span..index + 2 * span],
                )
            {
                skipped = span;
                break;
            }
        }
        out.push(tokens[index]);
        index += 1 + skipped;
    }
    out.join(" ")
}

/// Remove spaces around characters that never take one, and around a `.` that is
/// between two word characters (an address, not a sentence break).
fn tighten(text: &str, marks: &[char]) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut previous: Option<char> = None;
    for (index, character) in characters.iter().copied().enumerate() {
        if character == ' ' {
            let after = characters.get(index + 1).copied();
            let tight_after = after.map(|next| marks.contains(&next)).unwrap_or(false);
            let tight_before = previous.map(|prev| marks.contains(&prev)).unwrap_or(false);
            // "example . com" is an address; ". " between sentences is not.
            let tight_dot = after == Some('.')
                && previous.map(|prev| prev.is_alphanumeric()).unwrap_or(false)
                && characters
                    .get(index + 2)
                    .map(|next| next.is_alphanumeric())
                    .unwrap_or(false);
            if tight_after || tight_before || tight_dot {
                continue;
            }
        }
        out.push(character);
        previous = Some(character);
    }
    out
}

/// Whitespace and punctuation hygiene: one space between words, no space before
/// a terminal, no runs of a repeated mark, no leading punctuation left behind by
/// a removed filler.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut previous: Option<char> = None;

    for character in text.chars() {
        if character == '\n' {
            if out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
            pending_space = false;
            previous = Some('\n');
            continue;
        }
        if character.is_whitespace() {
            pending_space = true;
            continue;
        }
        let closes = matches!(
            character,
            ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '%'
        );
        let opens = matches!(previous, Some('(') | Some('[') | Some('\n'));
        if pending_space && !out.is_empty() && !closes && !opens {
            out.push(' ');
        }
        pending_space = false;
        let repeats_mark =
            matches!(character, ',' | ';' | ':' | '!') && previous == Some(character);
        if repeats_mark {
            continue;
        }
        out.push(character);
        previous = Some(character);
    }

    // A filler at the start of an utterance leaves its comma behind.
    out.trim()
        .trim_start_matches([',', ';', ':', '.', '!'])
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config() -> Config {
        Config::default()
    }

    fn text(raw: &str, config: &Config) -> String {
        match apply(raw, config) {
            Outcome::Text(text) => text,
            Outcome::Command(_) => panic!("expected text from {raw:?}"),
        }
    }

    #[test]
    fn fillers_go_and_the_punctuation_they_leave_behind_does_not() {
        assert_eq!(text("um, so the thing is", &config()), "so the thing is");
        assert_eq!(text("I was, uh, thinking", &config()), "I was, thinking");
        // A filler inside a word is not a filler.
        assert_eq!(text("the umbrella", &config()), "the umbrella");
    }

    #[test]
    fn a_repeated_run_collapses_to_one() {
        assert_eq!(text("the the file", &config()), "the file");
        assert_eq!(
            text("I think I think it works", &config()),
            "I think it works"
        );
        // Legitimate repetition in a longer phrase survives.
        assert_eq!(text("very very good", &config()), "very good");
    }

    #[test]
    fn spacing_and_stray_punctuation_are_tidied() {
        assert_eq!(text("hello  ,  world .", &config()), "hello, world.");
        assert_eq!(text(", so anyway", &config()), "so anyway");
        assert_eq!(text("well,, then", &config()), "well, then");
    }

    #[test]
    fn spoken_punctuation_is_opt_in() {
        let mut config = config();
        // Off by default: people say "comma" in prose.
        assert_eq!(text("hello comma world", &config), "hello comma world");

        config.punctuation_commands = true;
        assert_eq!(text("hello comma world", &config), "hello, world");
        assert_eq!(text("really question mark", &config), "really?");
        assert_eq!(text("one new paragraph two", &config), "one\n\ntwo");
    }

    #[test]
    fn spoken_addresses_become_addresses() {
        assert_eq!(
            text("mail me at sign sean at sign example dot com", &config()),
            "mail me@sean@example.com"
        );
        assert_eq!(
            text("go to example dot dev", &config()),
            "go to example.dev"
        );
        // A dot that is not a domain stays a sentence.
        assert_eq!(text("dot. fine", &config()), "dot. fine");
    }

    #[test]
    fn replacements_are_whole_words_and_case_insensitive() {
        let mut config = config();
        config.replacements = vec![crate::config::Replacement {
            from: "myware".to_string(),
            to: "MyWare Voice".to_string(),
        }];
        assert_eq!(text("open myware now", &config), "open MyWare Voice now");
        // Not a substring match.
        assert_eq!(text("mywarehouse", &config), "mywarehouse");
    }

    #[test]
    fn a_snippet_expands_on_an_exact_match_only() {
        let mut config = config();
        config.snippets = vec![crate::config::Snippet {
            trigger: "my calendar link".to_string(),
            text: "https://cal.example/sean".to_string(),
        }];

        assert_eq!(
            text("my calendar link", &config),
            "https://cal.example/sean"
        );
        assert_eq!(
            text("My calendar link.", &config),
            "https://cal.example/sean"
        );
        // A sentence that merely contains it is a sentence.
        assert_eq!(
            text("here is my calendar link for you", &config),
            "here is my calendar link for you"
        );
    }

    #[test]
    fn a_command_fires_only_on_the_whole_utterance() {
        assert_eq!(
            apply("scratch that", &config()),
            Outcome::Command(Command::Discard)
        );
        assert_eq!(
            apply("Scratch that.", &config()),
            Outcome::Command(Command::Discard)
        );
        assert_eq!(
            apply("quit myware", &config()),
            Outcome::Command(Command::Quit)
        );
        assert!(matches!(
            apply("I said scratch that earlier", &config()),
            Outcome::Text(_)
        ));

        let mut config = config();
        config.commands = false;
        assert!(matches!(apply("scratch that", &config), Outcome::Text(_)));
    }

    #[test]
    fn a_trailing_instruction_reshapes_the_utterance() {
        // The suffix is an instruction, not something to keep.
        assert_eq!(
            text("buy milk. book a table. call Ada. as a list", &config()),
            "\u{2022} buy milk.\n\u{2022} book a table.\n\u{2022} call Ada."
        );
        assert_eq!(
            text("the meeting moved to four on one line", &config()),
            "the meeting moved to four"
        );
        // With voice commands off, it is just words again.
        let mut plain = config();
        plain.commands = false;
        assert_eq!(text("buy milk as a list", &plain), "buy milk as a list");
    }

    #[test]
    fn a_rule_edited_in_the_config_is_the_rule_that_runs() {
        use crate::config::{CommandRule, SpokenRule};
        let mut config = config();

        // Fillers are a list, and replacing it replaces the behaviour.
        config.fillers = vec!["basically".to_string()];
        assert_eq!(text("basically the plan", &config), "the plan");
        assert_eq!(text("um the plan", &config), "um the plan");

        config.punctuation_commands = true;
        config.punctuation = vec![SpokenRule {
            say: "semi".to_string(),
            becomes: "; ".to_string(),
        }];
        assert_eq!(text("one semi two", &config), "one; two");

        config.command_rules = vec![CommandRule {
            say: "belay that".to_string(),
            action: "discard".to_string(),
        }];
        assert_eq!(
            apply("belay that", &config),
            Outcome::Command(Command::Discard)
        );

        config.shape_rules = vec![CommandRule {
            say: "as points".to_string(),
            action: "bullets".to_string(),
        }];
        assert_eq!(
            text("first. second. as points", &config),
            "\u{2022} first.\n\u{2022} second."
        );
    }

    #[test]
    fn a_comma_in_the_matching_field_names_alternatives() {
        use crate::config::{CommandRule, Replacement, Snippet, SpokenRule};
        let mut config = config();

        config.punctuation_commands = true;
        config.punctuation = vec![SpokenRule {
            say: "bang, exclamation mark".to_string(),
            becomes: "! ".to_string(),
        }];
        assert_eq!(text("one bang two", &config), "one! two");
        assert_eq!(text("one exclamation mark two", &config), "one! two");

        config.formatting_rules = vec![SpokenRule {
            say: "at, atsign".to_string(),
            becomes: "@".to_string(),
        }];
        assert_eq!(text("sean atsign example", &config), "sean@example");

        config.command_rules = vec![CommandRule {
            say: "scratch that, never mind".to_string(),
            action: "discard".to_string(),
        }];
        assert_eq!(
            apply("never mind", &config),
            Outcome::Command(Command::Discard)
        );

        config.shape_rules = vec![CommandRule {
            say: "as a list, as bullets".to_string(),
            action: "bullets".to_string(),
        }];
        assert_eq!(
            text("one. two. as bullets", &config),
            "\u{2022} one.\n\u{2022} two."
        );

        config.snippets = vec![Snippet {
            trigger: "my link, my calendar".to_string(),
            text: "https://cal.example".to_string(),
        }];
        assert_eq!(text("my calendar", &config), "https://cal.example");

        config.replacements = vec![Replacement {
            from: "myware, mywar".to_string(),
            to: "MyWare".to_string(),
        }];
        assert_eq!(text("call mywar now", &config), "call MyWare now");
    }

    #[test]
    fn the_output_side_of_a_rule_is_never_split() {
        use crate::config::SpokenRule;
        let mut config = config();
        config.punctuation_commands = true;
        // "comma" is a spoken name, not a separator here, and the replacement it
        // produces contains a comma and a space.
        assert_eq!(text("hello comma world", &config), "hello, world");
        config.punctuation = vec![SpokenRule {
            say: "ellipsis".to_string(),
            becomes: "a, b".to_string(),
        }];
        assert_eq!(text("wait ellipsis now", &config), "wait a, b now");
    }

    #[test]
    fn an_unknown_action_is_neither_text_nor_a_command() {
        use crate::config::CommandRule;
        let mut config = config();
        config.command_rules = vec![CommandRule {
            say: "make it fancy".to_string(),
            action: "fancy".to_string(),
        }];
        // Not a command this build knows, so it stays words on the way to the
        // pipeline rather than being swallowed.
        assert!(matches!(apply("make it fancy", &config), Outcome::Text(_)));
    }

    #[test]
    fn a_terminal_gets_one_line() {
        assert_eq!(flatten("one\n\ntwo"), "one two");
        assert_eq!(flatten("one \n two "), "one two");
    }

    #[test]
    fn cleanup_can_be_turned_off_entirely() {
        let mut config = config();
        config.cleanup = false;
        // The cleanup rules are off, but the spacing pass still runs: it is what
        // keeps a chunk boundary from producing a double space.
        assert_eq!(
            text("um, so  the the thing", &config),
            "um, so the the thing"
        );
    }

    #[test]
    fn a_url_in_the_text_is_not_mangled() {
        let mut config = config();
        config.replacements = vec![crate::config::Replacement {
            from: "dot".to_string(),
            to: "DOT".to_string(),
        }];
        // The address pass runs first and wins: a dictionary rule must not be able
        // to break an address it never saw as a word.
        assert_eq!(text("see example dot com", &config), "see example.com");
    }
}
