//! Inbound compliance filter: screens third-party text before the node keys it.
//!
//! Inbound replies are written by people who are not licensed operators, but the
//! licensee is responsible for what the node transmits (47 CFR 97.113). A word list
//! is easy to defeat and over-blocks, so Claude makes the judgment call, under two
//! constraints from the design:
//!
//! - Conservative and transparent. The model never rewrites text. It only names
//!   exact spans to redact, or drops the message; the node itself replaces each span
//!   with the visible token `REDACTED`, so the field operator knows scrubbing
//!   happened and everything else arrives verbatim. If a named span is not found
//!   in the text, the whole message is withheld rather than guessed at.
//! - An explicit threshold, stated to the model, so casual messages are not
//!   over-redacted.
//!
//! Failures never let text through: an API error leaves the message unscreened (it
//! is retried later and is never transmitted meanwhile), and a model refusal
//! withholds it.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

/// Replacement for every redacted span.
pub const MARKER: &str = "REDACTED";
/// What is keyed in place of a dropped message.
pub const WITHHELD: &str = "MSG WITHHELD BY FILTER";

const POLICY: &str = "\
You screen short personal messages before an amateur radio station transmits them \
in Morse code on the air. The station licensee is legally responsible for anything \
transmitted. Under US rules (47 CFR 97.113) the station must not transmit:
- obscene, indecent or profane words or language;
- messages encoded or written in a cipher to obscure their meaning;
- advertising or commercial solicitation (spam, promotions);
- music, or messages in furtherance of a criminal act.
Ordinary family and friends' messages are fine and are the normal case: plans, \
logistics, news, affection, mild slang, jokes, health updates, and mentions of work \
or money in a personal context. Do not redact names, places, times or numbers. Do not \
judge tone or politeness. Only act when the text clearly crosses one of the lines above.

Decide one action:
- \"keep\": nothing to change (the usual answer).
- \"redact\": the message is fine except for specific words or phrases. List each one \
in `spans`, copied exactly as it appears in the message (same letters and spacing), \
as short as possible.
- \"drop\": the whole message is not fit to transmit (for example spam, or mostly \
obscene).
Never rewrite, paraphrase or summarise the message. Give a brief `reason`.";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Keep,
    Redact,
    Drop,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Verdict {
    pub action: Action,
    #[serde(default)]
    pub spans: Vec<String>,
    #[serde(default)]
    pub reason: String,
}

/// The text to key for a message, given the model's verdict.
pub fn apply(text: &str, v: &Verdict) -> String {
    match v.action {
        Action::Keep => text.to_string(),
        Action::Drop => WITHHELD.to_string(),
        Action::Redact => {
            let mut out = text.to_string();
            let mut applied = false;
            for span in &v.spans {
                let span = span.trim();
                if span.is_empty() {
                    continue;
                }
                if !out.contains(span) {
                    // The model named something that is not there: do not guess.
                    return WITHHELD.to_string();
                }
                out = out.replace(span, MARKER);
                applied = true;
            }
            if !applied {
                // Flagged for redaction but nothing named to redact: withhold rather
                // than send the flagged text unchanged.
                return WITHHELD.to_string();
            }
            out
        }
    }
}

pub struct ClaudeFilter {
    agent: ureq::Agent,
    api_key: String,
    model: String,
    extra_policy: String,
    base: String,
}

impl ClaudeFilter {
    pub fn new(cfg: &crate::config::Filter) -> Result<Self> {
        let api_key = std::env::var(&cfg.api_key_env)
            .with_context(|| format!("environment variable {} is not set", cfg.api_key_env))?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into();
        Ok(Self {
            agent,
            api_key,
            model: cfg.model.clone(),
            extra_policy: cfg.extra_policy.clone(),
            base: "https://api.anthropic.com".into(),
        })
    }

    /// Screen one message. `text` must already be in its on-air form (sanitized,
    /// uppercase) so that spans match exactly what would be keyed.
    pub fn screen(&self, from: &str, text: &str) -> Result<Verdict> {
        let mut system = POLICY.to_string();
        if !self.extra_policy.trim().is_empty() {
            system.push_str("\n\nAdditional station policy:\n");
            system.push_str(&self.extra_policy);
        }
        let body = json!({
            "model": self.model,
            "max_tokens": 16000,
            "fallbacks": "default",
            "system": system,
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "action": {"type": "string", "enum": ["keep", "redact", "drop"]},
                            "spans": {"type": "array", "items": {"type": "string"}},
                            "reason": {"type": "string"}
                        },
                        "required": ["action", "spans", "reason"],
                        "additionalProperties": false
                    }
                }
            },
            "messages": [{
                "role": "user",
                "content": format!("Message from {from}, to be transmitted:\n<message>\n{text}\n</message>")
            }]
        });
        let mut resp = self
            .agent
            .post(&format!("{}/v1/messages", self.base))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "server-side-fallback-2026-07-01")
            .header("content-type", "application/json")
            .send_json(&body)
            .context("Claude API request")?;
        let v: Value = resp.body_mut().read_json()?;
        parse_response(&v)
    }
}

/// Interpret a Messages API response. A refusal withholds the message.
pub fn parse_response(v: &Value) -> Result<Verdict> {
    if v["stop_reason"] == "refusal" {
        return Ok(Verdict {
            action: Action::Drop,
            spans: vec![],
            reason: "model declined to screen".into(),
        });
    }
    if v["stop_reason"] == "max_tokens" {
        bail!("filter response truncated");
    }
    let text = v["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<String>();
    serde_json::from_str(&text).with_context(|| format!("filter returned non-JSON: {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(action: Action, spans: &[&str]) -> Verdict {
        Verdict {
            action,
            spans: spans.iter().map(|s| s.to_string()).collect(),
            reason: String::new(),
        }
    }

    #[test]
    fn applies_verdicts_without_rewriting() {
        let t = "SEE YOU SUN DARN TRAFFIC";
        assert_eq!(apply(t, &verdict(Action::Keep, &[])), t);
        assert_eq!(
            apply(t, &verdict(Action::Redact, &["DARN"])),
            "SEE YOU SUN REDACTED TRAFFIC"
        );
        assert_eq!(apply(t, &verdict(Action::Drop, &[])), WITHHELD);
        // A span that is not in the text withholds the message.
        assert_eq!(apply(t, &verdict(Action::Redact, &["SEE YA"])), WITHHELD);
        // A redact verdict with no usable span withholds rather than passes.
        assert_eq!(apply(t, &verdict(Action::Redact, &[])), WITHHELD);
        assert_eq!(apply(t, &verdict(Action::Redact, &[" ", ""])), WITHHELD);
    }

    #[test]
    fn parses_api_responses() {
        let ok: Value = serde_json::from_str(
            r#"{"stop_reason":"end_turn","content":[{"type":"thinking","thinking":""},{"type":"text","text":"{\"action\":\"keep\",\"spans\":[],\"reason\":\"ordinary\"}"}]}"#,
        )
        .unwrap();
        assert_eq!(parse_response(&ok).unwrap().action, Action::Keep);
        let refusal: Value =
            serde_json::from_str(r#"{"stop_reason":"refusal","content":[]}"#).unwrap();
        assert_eq!(parse_response(&refusal).unwrap().action, Action::Drop);
        let cut: Value =
            serde_json::from_str(r#"{"stop_reason":"max_tokens","content":[]}"#).unwrap();
        assert!(parse_response(&cut).is_err());
    }
}
