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
#[non_exhaustive]
pub enum DegradeError {
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
pub fn encode_request(tier: ToolSupport, mut req: LlmRequest) -> LlmRequest {
    match tier {
        ToolSupport::Native => {
            if !req.tools.iter().any(|t| t.name == PLAN_TOOL) {
                req.tools.push(ToolDef {
                    name: PLAN_TOOL.to_owned(),
                    description: "Propose a plan of steps for the user to review and approve."
                        .to_owned(),
                });
            }
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
        ToolSupport::Native => "",
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
pub fn decode_plan(
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
        (ToolSupport::Text, LlmResponse::Text(t)) => parse_json_object(strip_code_fence(t)),
        (ToolSupport::JsonSchema, LlmResponse::Text(t)) => parse_json_object(t),
        (_, LlmResponse::ToolCall { .. }) => Err(DegradeError::NotAPlanCall),
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

/// Find and parse the first balanced top-level `{ … }` JSON object in `text`.
fn parse_json_object(text: &str) -> Result<serde_json::Value, DegradeError> {
    let bytes = text.as_bytes();
    let start = text.find('{').ok_or(DegradeError::NoJson)?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let slice = &text[start..=i];
                    return serde_json::from_str(slice).map_err(|_| DegradeError::BadJson);
                }
            }
            _ => {}
        }
    }
    Err(DegradeError::NoJson)
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
    fn missing_or_malformed_json_errors() {
        assert_eq!(
            parse_json_object("no object here"),
            Err(DegradeError::NoJson)
        );
        assert_eq!(parse_json_object("{not valid"), Err(DegradeError::NoJson));
        assert_eq!(parse_json_object("{\"a\": }"), Err(DegradeError::BadJson));
    }
}
