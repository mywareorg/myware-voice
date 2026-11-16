//! Rules proposed from what was actually dictated.
//!
//! The text pass is deterministic on purpose (see [`crate::text`]), so the one
//! job an LLM is trusted with here is *proposal*: read the history, notice the
//! corrections a user keeps making by hand, and write them as rules. Nothing
//! this module returns is applied. A proposal is a row in a settings window that
//! a person accepts or dismisses, and the accepted ones go through the same
//! tables the hand-written rules use.
//!
//! That split is what keeps the promise the rest of the app makes: a rule either
//! fires or it does not, and the model is never in the path of a paste.
//!
//! The request asks Ollama for `"format": "json"` so the reply is a document
//! rather than prose, but [`parse`] still tolerates a fenced block or a bare
//! array, because a model that ignores the format must not crash the window.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::history;
use crate::ollama::{self, Client};

/// How many transcripts one prompt carries.
const MAX_ENTRIES: usize = 60;

/// A single transcript longer than this is cut, so one rambling entry cannot
/// crowd out the other fifty.
const MAX_ENTRY_CHARS: usize = 400;

/// Total transcript characters in one prompt. A local model has a context
/// window, and blowing past it fails the request rather than shortening it.
const MAX_TRANSCRIPT_CHARS: usize = 6000;

/// Existing rules included for context, per table.
const MAX_CONFIGURED: usize = 40;

/// Proposals kept from one reply. A window with forty rows is a wall, not a
/// suggestion.
pub const MAX_PROPOSALS: usize = 8;

/// Which table a proposal would be written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A whole-word substitution: a name the decoder keeps getting wrong.
    Replacement,
    /// A phrase that expands to fixed text.
    Snippet,
}

impl Kind {
    pub fn arg(self) -> &'static str {
        match self {
            Kind::Replacement => "replacement",
            Kind::Snippet => "snippet",
        }
    }

    pub fn from_arg(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "replacement" => Some(Kind::Replacement),
            "snippet" => Some(Kind::Snippet),
            _ => None,
        }
    }
}

/// One rule the model thinks the user would have wanted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub kind: Kind,
    /// What was heard. For a replacement this is the wrong form; for a snippet
    /// it is the trigger phrase.
    pub say: String,
    /// What it should become.
    pub becomes: String,
    /// Why the model proposed it, in one line, shown muted under the row.
    pub reason: String,
}

impl Serialize for Kind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.arg())
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Kind::from_arg(&value)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown rule kind {value:?}")))
    }
}

/// The prompt for one suggestion run: the rules that exist, then the words that
/// were actually said.
///
/// Public and pure so the shape of what leaves the machine is testable without a
/// server. Everything in it is either a transcript the user chose to keep or a
/// rule they wrote.
pub fn prompt(entries: &[String], config: &Config) -> String {
    let mut out = String::new();
    out.push_str(
        "You tune a dictation cleanup pipeline. The transcripts below were produced by a \
         speech model and delivered with the rules listed. Propose at most ",
    );
    out.push_str(&MAX_PROPOSALS.to_string());
    out.push_str(
        " rules that would fix mistakes that recur.\n\n\
         A \"replacement\" maps a word or phrase the speech model got wrong to the correct \
         form. It matches whole words, case-insensitively, and is the right kind for a name, \
         an acronym or a homophone.\n\
         A \"snippet\" maps a phrase the user says verbatim to fixed text. Use it only when a \
         whole phrase repeats; do not use it for a correction.\n\n\
         Rules:\n\
         - Prefer an exact recurring mistake over a matter of style.\n\
         - Do not propose filler removal, punctuation or formatting: those are handled already.\n\
         - Do not propose anything already configured.\n\
         - `say` is short, and lowercase as it was heard.\n\
         - `becomes` is what the user would have typed.\n\
         - `reason` is one line, at most twelve words.\n\
         - If there is nothing worth proposing, return an empty list.\n\n\
         Reply with JSON only, in this shape:\n\
         {\"proposals\":[{\"kind\":\"replacement\",\"say\":\"...\",\"becomes\":\"...\",\"reason\":\"...\"}]}\n",
    );

    let mut configured = String::new();
    let replacements: Vec<String> = config
        .replacements
        .iter()
        .filter(|rule| !rule.from.trim().is_empty())
        .take(MAX_CONFIGURED)
        .map(|rule| format!("{} -> {}", rule.from.trim(), rule.to.trim()))
        .collect();
    if !replacements.is_empty() {
        configured.push_str("replacements: ");
        configured.push_str(&replacements.join("; "));
        configured.push('\n');
    }
    let snippets: Vec<String> = config
        .snippets
        .iter()
        .filter(|snippet| !snippet.trigger.trim().is_empty())
        .take(MAX_CONFIGURED)
        .map(|snippet| format!("{} -> {}", snippet.trigger.trim(), snippet.text.trim()))
        .collect();
    if !snippets.is_empty() {
        configured.push_str("snippets: ");
        configured.push_str(&snippets.join("; "));
        configured.push('\n');
    }
    if !configured.is_empty() {
        out.push_str("\nAlready configured:\n");
        out.push_str(&configured);
    }

    out.push_str("\nTranscripts, oldest first:\n");
    let mut budget = MAX_TRANSCRIPT_CHARS;
    let entries = if entries.len() > MAX_ENTRIES {
        &entries[entries.len() - MAX_ENTRIES..]
    } else {
        entries
    };
    for entry in entries {
        if budget == 0 {
            break;
        }
        let line = truncate(entry.trim(), MAX_ENTRY_CHARS);
        if line.is_empty() {
            continue;
        }
        let cost = line.chars().count() + 3;
        if cost > budget {
            break;
        }
        budget -= cost;
        out.push_str("- ");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Read the model's reply as proposals, discarding anything malformed.
///
/// A single bad row is dropped rather than failing the batch: a model that gets
/// seven rules right and one wrong should still be useful.
pub fn parse(raw: &str) -> Result<Vec<Proposal>> {
    let json = extract_json(raw).context("suggest: the reply had no JSON in it")?;

    #[derive(Deserialize)]
    struct Document {
        #[serde(default)]
        proposals: Vec<Row>,
    }
    #[derive(Deserialize)]
    struct Row {
        kind: String,
        #[serde(default)]
        say: String,
        #[serde(default)]
        becomes: String,
        #[serde(default)]
        reason: String,
    }

    let rows: Vec<Row> = match serde_json::from_str::<Document>(json) {
        Ok(document) => document.proposals,
        Err(_) => serde_json::from_str::<Vec<Row>>(json)
            .context("suggest: the reply was not a proposal document")?,
    };

    let mut proposals = Vec::new();
    for row in rows {
        let Some(kind) = Kind::from_arg(&row.kind) else {
            continue;
        };
        let say = one_line(&row.say);
        let becomes = row.becomes.trim().to_string();
        if say.is_empty() || becomes.is_empty() {
            continue;
        }
        // A rule that changes nothing is noise, and `from == to` would still
        // cost a pass over every transcript at paste time.
        if say.eq_ignore_ascii_case(&becomes) {
            continue;
        }
        if proposals
            .iter()
            .any(|existing: &Proposal| existing.say.eq_ignore_ascii_case(&say))
        {
            continue;
        }
        proposals.push(Proposal {
            kind,
            say,
            becomes,
            reason: one_line(&row.reason),
        });
        if proposals.len() == MAX_PROPOSALS {
            break;
        }
    }
    Ok(proposals)
}

/// The model one suggestion run may use.
///
/// An explicit name in `ollama_model` is honoured whatever it is: pointing this
/// at a model served from `ollama.com` is the user's decision to make, and
/// overriding a line they wrote would be the app deciding for them.
///
/// With no name there is no decision to respect, so the choice is drawn only
/// from what the server reads off this machine's disk. Picking a remote model
/// there would send a transcript somewhere on the strength of list order.
///
/// Pure over the model list so the policy is testable without a server.
pub fn choose_model(models: &[ollama::Model], preferred: &str) -> Result<String> {
    let preferred = preferred.trim();
    if models.is_empty() {
        bail!("no models installed; run `ollama pull <model>`");
    }
    if !preferred.is_empty() {
        return match models.iter().find(|model| model.name == preferred) {
            Some(_) => Ok(preferred.to_string()),
            None => bail!("{preferred} is not installed on the local server"),
        };
    }
    match models.iter().find(|model| !model.is_remote()) {
        Some(model) => Ok(model.name.clone()),
        None => bail!(
            "every installed model is served from {}; none is on this machine. Run `ollama pull <model>`, or set `ollama_model` to name a cloud model deliberately.",
            models[0].host()
        ),
    }
}

/// Read the history and ask `model` for rules.
///
/// `days` is the window of history to read, and matches what the history pane
/// shows so the two cannot disagree about what the model saw. An empty `model`
/// means "the first one on disk"; [`choose_model`] refuses anything the server
/// fetches over the network.
pub fn suggest(
    client: &Client,
    model: &str,
    days: u32,
    config: &Config,
) -> Result<Vec<Proposal>> {
    let models = client
        .models_detailed()
        .context("suggest: list the models the local server has")?;
    let model = choose_model(&models, model)?;
    let entries = history::recent(days.max(1))?;
    let texts: Vec<String> = entries
        .into_iter()
        .map(|entry| entry.text)
        .filter(|text| !text.trim().is_empty())
        .collect();
    if texts.is_empty() {
        bail!(
            "suggest: no history to read; turn history on in Settings and dictate something first"
        );
    }

    let request = prompt(&texts, config);
    let raw = client.generate_json(&model, &request)?;
    let proposals = parse(&raw)?;

    // A rule that already exists is not a suggestion. Filtering here rather than
    // at render time means re-running the same prompt does not return the same
    // rows the user already accepted.
    let known_from = config.replacements.iter().map(|rule| rule.from.trim());
    let known_trigger = config.snippets.iter().map(|snippet| snippet.trigger.trim());
    let known: Vec<String> = known_from
        .chain(known_trigger)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_lowercase())
        .collect();
    Ok(proposals
        .into_iter()
        .filter(|proposal| !known.contains(&proposal.say.to_lowercase()))
        .collect())
}

/// The first JSON document in `raw`, fences and prose stripped.
fn extract_json(raw: &str) -> Option<&str> {
    let start = raw.find(['{', '['])?;
    let rest = &raw[start..];
    let end = rest.rfind(['}', ']'])?;
    Some(&rest[..=end])
}

/// Collapse a field to a single trimmed line.
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut a string to at most `max` characters on a word boundary.
fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max).collect();
    if let Some(space) = out.rfind(' ') {
        out.truncate(space);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_rules() -> Config {
        let mut config = Config::default();
        config.replacements.push(crate::config::Replacement {
            from: "sean".to_string(),
            to: "Shaun".to_string(),
        });
        config
    }

    #[test]
    fn a_reply_with_prose_around_it_still_parses() {
        let raw = r#"Sure. Here are the rules:
```json
{"proposals":[{"kind":"replacement","say":"mywar","becomes":"MyWare","reason":"name is misheard"}]}
```"#;
        let proposals = parse(raw).expect("parsed");
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].kind, Kind::Replacement);
        assert_eq!(proposals[0].say, "mywar");
        assert_eq!(proposals[0].becomes, "MyWare");
    }

    #[test]
    fn a_bare_array_is_also_a_reply() {
        let raw = r#"[{"kind":"snippet","say":"my address","becomes":"12 Elm St","reason":"repeated"}]"#;
        let proposals = parse(raw).expect("parsed");
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].kind, Kind::Snippet);
    }

    #[test]
    fn malformed_rows_are_dropped_and_the_rest_kept() {
        let raw = r#"{"proposals":[
            {"kind":"punctuation","say":"comma","becomes":", ","reason":"unknown kind"},
            {"kind":"replacement","say":"","becomes":"x","reason":"empty"},
            {"kind":"replacement","say":"same","becomes":"same","reason":"no change"},
            {"kind":"replacement","say":"zed","becomes":"Zed","reason":" ok "},
            {"kind":"replacement","say":"ZED","becomes":"Zed","reason":"duplicate"}
        ]}"#;
        let proposals = parse(raw).expect("parsed");
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].say, "zed");
        assert_eq!(proposals[0].reason, "ok");
    }

    #[test]
    fn a_reply_with_no_json_is_an_error() {
        assert!(parse("I could not find anything to suggest.").is_err());
    }

    #[test]
    fn the_prompt_carries_the_rules_and_the_transcripts() {
        let config = config_with_rules();
        let text = prompt(&["um, mail sean".to_string()], &config);
        assert!(text.contains("\nAlready configured:\n"));
        assert!(text.contains("sean -> Shaun"));
        assert!(text.contains("- um, mail sean"));
        assert!(text.contains("\"proposals\""));
    }

    #[test]
    fn an_explicit_cloud_model_is_honoured_and_an_implicit_one_is_not() {
        let models = [
            ollama::Model {
                name: "big:70b".to_string(),
                remote_host: Some("https://ollama.com".to_string()),
            },
            ollama::Model {
                name: "llama3.2:3b".to_string(),
                remote_host: None,
            },
        ];
        // Unset: the first model on disk, not the first model listed. Picking the
        // cloud one here would be a choice nobody made.
        assert_eq!(choose_model(&models, "").expect("chosen"), "llama3.2:3b");
        // Named: honoured either way. Pointing the config at a cloud model is the
        // user's decision, and the app does not overrule a written line.
        assert_eq!(choose_model(&models, "llama3.2:3b").expect("chosen"), "llama3.2:3b");
        assert_eq!(choose_model(&models, "big:70b").expect("chosen"), "big:70b");
        // Named but absent is an error either way.
        let error = choose_model(&models, "nope").expect_err("refused");
        assert!(error.to_string().contains("not installed"), "{error}");
    }

    #[test]
    fn a_server_with_nothing_local_says_why_it_cannot_run() {
        let models = [ollama::Model {
            name: "glm-5.2:cloud".to_string(),
            remote_host: Some("https://ollama.com".to_string()),
        }];
        let error = choose_model(&models, "").expect_err("refused");
        assert!(error.to_string().contains("none is on this machine"), "{error}");
        // The way out is named, and it is the user's to take.
        assert!(error.to_string().contains("`ollama_model`"), "{error}");
    }

    #[test]
    fn an_empty_server_points_at_the_pull_command() {
        let error = choose_model(&[], "").expect_err("refused");
        assert!(error.to_string().contains("ollama pull"), "{error}");
    }

    #[test]
    fn a_long_transcript_is_cut_rather_than_dropped() {
        let long = "word ".repeat(500);
        let prompt = prompt(&[long], &Config::default());
        assert!(prompt.contains('…'));
        assert!(prompt.chars().count() < MAX_TRANSCRIPT_CHARS + 2000);
    }
}
