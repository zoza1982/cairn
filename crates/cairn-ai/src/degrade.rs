//! Tool-call **degradation**: adapting how a plan is requested from and parsed back out of a model,
//! according to the provider's [`ToolSupport`] tier.
//!
//! - [`ToolSupport::Native`] — the model has a real tool/function-call API. Tools are advertised in
//!   the request and the reply arrives as a structured [`LlmResponse::ToolCall`].
//! - [`ToolSupport::JsonSchema`] — no native tools, but reliable instruction following: the system
//!   prompt asks for a bare JSON object, returned as [`LlmResponse::Text`].
//! - [`ToolSupport::Text`] — weakest models: the system prompt asks for a fenced ```json block,
//!   extracted from the reply text.
//!
//! The whole module is pure and synchronous, so the three tiers are fully testable offline against a
//! scripted [`MockProvider`] — no network. The HTTP transport for the concrete Ollama/OpenAI-compat
//! providers is the integration step; this is the degradation logic those providers share.

use crate::provider::{LlmRequest, LlmResponse, ToolDef, ToolSupport};

/// The tool the model uses to return a plan. The only structured output the agent accepts.
pub(crate) const PLAN_TOOL: &str = "propose_plan";

/// Errors extracting a plan payload from a model response.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum DegradeError {
    /// The native reply was not a `propose_plan` tool call (some other tool, or plain text).
    #[error("model did not call propose_plan")]
    NotAPlanCall,
    /// No JSON object could be found in a text reply.
    #[error("no JSON object found in response")]
    NoJson,
    /// A JSON object was found but did not parse.
    #[error("malformed JSON in response")]
    BadJson,
}

/// Adapt a request for the given tier: native tier advertises the plan tool; the text tiers instead
/// fold an output-format instruction into the system prompt (and clear `tools`, which the model
/// can't use).
#[must_use]
pub(crate) fn encode_request(tier: ToolSupport, mut req: LlmRequest) -> LlmRequest {
    match tier {
        ToolSupport::Native => {
            // Only `propose_plan` is ever advertised — the model proposes; it never directly calls
            // an action tool (clear-then-add makes that explicit and symmetric with the text tiers).
            req.tools.clear();
            req.tools.push(ToolDef {
                name: PLAN_TOOL.to_owned(),
                description: "Propose a plan of steps for the user to review and approve."
                    .to_owned(),
            });
            req
        }
        ToolSupport::JsonSchema | ToolSupport::Text => {
            req.tools.clear();
            req.system = Some(augment_system(req.system.as_deref(), tier));
            req
        }
    }
}

/// The output-format instruction appended to the system prompt for the non-native tiers.
fn augment_system(base: Option<&str>, tier: ToolSupport) -> String {
    let instruction = match tier {
        ToolSupport::JsonSchema => {
            "Respond with ONLY a single JSON object (no prose, no code fence) of the form: \
             {\"summary\": string, \"steps\": [{\"tool\": string, \"input\": object, \
             \"description\": string}, ...]}."
        }
        ToolSupport::Text => {
            "Respond with a single fenced code block ```json ... ``` containing a JSON object of the \
             form: {\"summary\": string, \"steps\": [{\"tool\": string, \"input\": object, \
             \"description\": string}, ...]}. Output nothing outside the fence."
        }
        ToolSupport::Native => unreachable!("augment_system is only called for the text tiers"),
    };
    match base {
        Some(b) if !b.is_empty() => format!("{b}\n\n{instruction}"),
        _ => instruction.to_owned(),
    }
}

/// Extract the raw plan payload (the `propose_plan` input object) from a response for the tier.
///
/// # Errors
/// [`DegradeError`] if the response does not carry a usable plan object.
pub(crate) fn decode_plan(
    tier: ToolSupport,
    resp: &LlmResponse,
) -> Result<serde_json::Value, DegradeError> {
    match (tier, resp) {
        (ToolSupport::Native, LlmResponse::ToolCall { name, input }) if name == PLAN_TOOL => {
            Ok(input.clone())
        }
        (ToolSupport::Native, _) => Err(DegradeError::NotAPlanCall),
        // The text tiers may still come back as a tool call (a more capable model than declared);
        // accept that, otherwise extract a JSON object from the text.
        (_, LlmResponse::ToolCall { name, input }) if name == PLAN_TOOL => Ok(input.clone()),
        (ToolSupport::Text, LlmResponse::Text(t)) => {
            // Prefer the first fenced block, but fall back to scanning the whole reply if that block
            // held no JSON (a weak model may emit a throwaway fence before the real one).
            parse_json_object(strip_code_fence(t)).or_else(|e| match e {
                DegradeError::NoJson => parse_json_object(t),
                other => Err(other),
            })
        }
        (ToolSupport::JsonSchema, LlmResponse::Text(t)) => parse_json_object(t),
        (ToolSupport::JsonSchema | ToolSupport::Text, LlmResponse::ToolCall { .. }) => {
            Err(DegradeError::NotAPlanCall)
        }
    }
}

/// Pull the contents of the first fenced code block (```json … ``` or ``` … ```), or return the
/// whole input if there is no fence.
fn strip_code_fence(text: &str) -> &str {
    let Some(open) = text.find("```") else {
        return text;
    };
    // Skip the opening fence and an optional language tag up to the end of that line.
    let after = &text[open + 3..];
    let body_start = after.find('\n').map_or(0, |nl| nl + 1);
    let body = &after[body_start..];
    match body.find("```") {
        Some(close) => &body[..close],
        None => body,
    }
}

/// Find and parse the **first** JSON object embedded in `text` (matching the "respond with only a
/// single JSON object" instruction; leading prose before the `{` and trailing prose after the object
/// are tolerated). Delegates the actual parse to `serde_json`'s streaming deserializer, so string
/// escaping, nesting, and UTF-8 are handled correctly with no hand-rolled scanning.
fn parse_json_object(text: &str) -> Result<serde_json::Value, DegradeError> {
    let start = text.find('{').ok_or(DegradeError::NoJson)?;
    // `StreamDeserializer::next` reads exactly one JSON value from `text[start..]` and stops, so any
    // trailing prose is ignored. `None` is unreachable after a `{`, but map it defensively.
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<serde_json::Value>()
        .next()
        .ok_or(DegradeError::BadJson)?
        .map_err(|_| DegradeError::BadJson)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_json() -> serde_json::Value {
        serde_json::json!({
            "summary": "archive logs",
            "steps": [{"tool": "list", "input": {"path": "/logs"}, "description": "list"}]
        })
    }

    #[test]
    fn native_encode_advertises_the_plan_tool() {
        let req = encode_request(ToolSupport::Native, LlmRequest::default());
        assert!(req.tools.iter().any(|t| t.name == PLAN_TOOL));
    }

    #[test]
    fn text_tiers_clear_tools_and_instruct_via_system() {
        for tier in [ToolSupport::JsonSchema, ToolSupport::Text] {
            let req = encode_request(
                tier,
                LlmRequest {
                    tools: vec![ToolDef {
                        name: "list".into(),
                        description: String::new(),
                    }],
                    ..Default::default()
                },
            );
            assert!(req.tools.is_empty(), "{tier:?} must not advertise tools");
            assert!(
                req.system.unwrap().contains("JSON"),
                "{tier:?} system instruction"
            );
        }
    }

    #[test]
    fn native_decode_reads_the_tool_call() {
        let resp = LlmResponse::ToolCall {
            name: PLAN_TOOL.to_owned(),
            input: plan_json(),
        };
        assert_eq!(
            decode_plan(ToolSupport::Native, &resp).unwrap(),
            plan_json()
        );
    }

    #[test]
    fn native_decode_rejects_non_plan_responses() {
        assert_eq!(
            decode_plan(ToolSupport::Native, &LlmResponse::Text("hi".into())),
            Err(DegradeError::NotAPlanCall)
        );
        let wrong = LlmResponse::ToolCall {
            name: "list".to_owned(),
            input: serde_json::json!({}),
        };
        assert_eq!(
            decode_plan(ToolSupport::Native, &wrong),
            Err(DegradeError::NotAPlanCall)
        );
    }

    #[test]
    fn json_schema_decode_extracts_bare_object() {
        let text = format!("Sure!\n{}\nDone.", plan_json());
        let resp = LlmResponse::Text(text);
        assert_eq!(
            decode_plan(ToolSupport::JsonSchema, &resp).unwrap(),
            plan_json()
        );
    }

    #[test]
    fn text_decode_extracts_from_fence() {
        let resp = LlmResponse::Text(format!("Here:\n```json\n{}\n```\n", plan_json()));
        assert_eq!(decode_plan(ToolSupport::Text, &resp).unwrap(), plan_json());
    }

    #[test]
    fn text_decode_handles_plain_fence_without_lang() {
        let resp = LlmResponse::Text(format!("```\n{}\n```", plan_json()));
        assert_eq!(decode_plan(ToolSupport::Text, &resp).unwrap(), plan_json());
    }

    #[test]
    fn json_object_parser_ignores_braces_inside_strings() {
        let text = r#"prefix {"summary": "a } b", "steps": []} suffix"#;
        let v = parse_json_object(text).unwrap();
        assert_eq!(v["summary"], "a } b");
    }

    #[test]
    fn json_object_parser_is_panic_free_on_adversarial_input() {
        // Multi-byte UTF-8 inside the object (pins the ASCII-only-indexing invariant).
        let v = parse_json_object(r#"{"日本語": "値🎉", "steps": []}"#).unwrap();
        assert_eq!(v["日本語"], "値🎉");
        // Escaped quote immediately followed by a brace inside a string.
        let v = parse_json_object(r#"{"k": "\"}", "steps": []}"#).unwrap();
        assert_eq!(v["k"], "\"}");
        // A stray closing brace before the first `{` must not underflow the depth counter.
        let v = parse_json_object(r#"} junk {"steps": []}"#).unwrap();
        assert!(v["steps"].is_array());
        // Trailing content after a complete object is ignored.
        let v = parse_json_object(r#"{"steps": []} and then prose"#).unwrap();
        assert!(v["steps"].is_array());
        // Boundary inputs return errors, never panic.
        assert_eq!(parse_json_object(""), Err(DegradeError::NoJson));
        // An unterminated object: a `{` was found but the parse fails → BadJson.
        assert_eq!(
            parse_json_object(r#"{"s": "abc"#),
            Err(DegradeError::BadJson)
        );
    }

    #[test]
    fn text_decode_falls_back_past_an_empty_first_fence() {
        // A throwaway code block before the real JSON fence must still resolve.
        let resp = LlmResponse::Text(format!(
            "```\nnot json here\n```\nthen:\n```json\n{}\n```",
            plan_json()
        ));
        assert_eq!(decode_plan(ToolSupport::Text, &resp).unwrap(), plan_json());
    }

    #[test]
    fn native_encode_advertises_only_the_plan_tool() {
        let req = encode_request(
            ToolSupport::Native,
            LlmRequest {
                tools: vec![ToolDef {
                    name: "delete".into(),
                    description: String::new(),
                }],
                ..Default::default()
            },
        );
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, PLAN_TOOL);
    }

    #[test]
    fn missing_or_malformed_json_errors() {
        // No `{` at all → nothing to parse.
        assert_eq!(
            parse_json_object("no object here"),
            Err(DegradeError::NoJson)
        );
        // A `{` was found but the content is not valid JSON → BadJson.
        assert_eq!(parse_json_object("{not valid"), Err(DegradeError::BadJson));
        assert_eq!(parse_json_object("{\"a\": }"), Err(DegradeError::BadJson));
    }

    #[test]
    fn augment_system_preserves_the_base_prompt() {
        let req = encode_request(
            ToolSupport::JsonSchema,
            LlmRequest {
                system: Some("You are Cairn.".to_owned()),
                ..Default::default()
            },
        );
        let sys = req.system.unwrap();
        assert!(sys.starts_with("You are Cairn."));
        assert!(sys.contains("\n\n"));
        assert!(sys.trim_end().ends_with("]}."));
    }

    #[test]
    fn native_encode_is_idempotent() {
        let once = encode_request(ToolSupport::Native, LlmRequest::default());
        let twice = encode_request(ToolSupport::Native, once);
        assert_eq!(twice.tools.len(), 1);
        assert_eq!(twice.tools[0].name, PLAN_TOOL);
    }
}
