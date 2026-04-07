use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::json::{JsonError, JsonValue};
use crate::usage::TokenUsage;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        tool_use_id: String,
        tool_name: String,
        output: String,
        is_error: bool,
    },
    UserInputRequest {
        request_id: String,
        prompt: String,
        options: Vec<String>,
        allow_freeform: bool,
    },
    UserInputResponse {
        request_id: String,
        content: String,
        selected_option: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub blocks: Vec<ContentBlock>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionAccountingStatus {
    #[default]
    Complete,
    PartialLegacyCompaction,
}

impl SessionAccountingStatus {
    #[must_use]
    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SessionTelemetry {
    pub compacted_usage: TokenUsage,
    pub compacted_turns: u32,
    #[serde(default)]
    pub accounting_status: SessionAccountingStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub version: u32,
    pub messages: Vec<ConversationMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<SessionTelemetry>,
}

#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Json(JsonError),
    Format(String),
}

impl Display for SessionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::Format(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<JsonError> for SessionError {
    fn from(value: JsonError) -> Self {
        Self::Json(value)
    }
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: 1,
            messages: Vec::new(),
            telemetry: None,
        }
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), SessionError> {
        fs::write(path, self.to_json().render())?;
        Ok(())
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let contents = fs::read_to_string(path)?;
        Self::from_json(&JsonValue::parse(&contents)?)
    }

    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let mut object = BTreeMap::new();
        object.insert(
            "version".to_string(),
            JsonValue::Number(i64::from(self.version)),
        );
        object.insert(
            "messages".to_string(),
            JsonValue::Array(
                self.messages
                    .iter()
                    .map(ConversationMessage::to_json)
                    .collect(),
            ),
        );
        if let Some(telemetry) = self.telemetry {
            object.insert("telemetry".to_string(), telemetry_to_json(telemetry));
        }
        JsonValue::Object(object)
    }

    pub fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("session must be an object".to_string()))?;
        let version = object
            .get("version")
            .and_then(JsonValue::as_i64)
            .ok_or_else(|| SessionError::Format("missing version".to_string()))?;
        let version = u32::try_from(version)
            .map_err(|_| SessionError::Format("version out of range".to_string()))?;
        let messages = object
            .get("messages")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| SessionError::Format("missing messages".to_string()))?
            .iter()
            .map(ConversationMessage::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let telemetry = object
            .get("telemetry")
            .map(telemetry_from_json)
            .transpose()?;
        Ok(Self {
            version,
            messages,
            telemetry,
        })
    }

    #[must_use]
    pub fn pending_user_input_request(&self) -> Option<PendingUserInputRequest> {
        let mut pending = Vec::new();
        for message in &self.messages {
            for block in &message.blocks {
                match block {
                    ContentBlock::UserInputRequest {
                        request_id,
                        prompt,
                        options,
                        allow_freeform,
                    } => pending.push(PendingUserInputRequest {
                        request_id: request_id.clone(),
                        prompt: prompt.clone(),
                        options: options.clone(),
                        allow_freeform: *allow_freeform,
                    }),
                    ContentBlock::UserInputResponse { request_id, .. } => {
                        pending.retain(|request| request.request_id != *request_id);
                    }
                    ContentBlock::Text { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. } => {}
                }
            }
        }
        pending.into_iter().next()
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversationMessage {
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text { text: text.into() }],
            usage: None,
        }
    }

    #[must_use]
    pub fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::Assistant,
            blocks,
            usage: None,
        }
    }

    #[must_use]
    pub fn assistant_with_usage(blocks: Vec<ContentBlock>, usage: Option<TokenUsage>) -> Self {
        Self {
            role: MessageRole::Assistant,
            blocks,
            usage,
        }
    }

    #[must_use]
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                tool_name: tool_name.into(),
                output: output.into(),
                is_error,
            }],
            usage: None,
        }
    }

    #[must_use]
    pub fn user_input_response(
        request_id: impl Into<String>,
        content: impl Into<String>,
        selected_option: Option<String>,
    ) -> Self {
        Self {
            role: MessageRole::User,
            blocks: vec![ContentBlock::UserInputResponse {
                request_id: request_id.into(),
                content: content.into(),
                selected_option,
            }],
            usage: None,
        }
    }

    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let mut object = BTreeMap::new();
        object.insert(
            "role".to_string(),
            JsonValue::String(
                match self.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Tool => "tool",
                }
                .to_string(),
            ),
        );
        object.insert(
            "blocks".to_string(),
            JsonValue::Array(self.blocks.iter().map(ContentBlock::to_json).collect()),
        );
        if let Some(usage) = self.usage {
            object.insert("usage".to_string(), usage_to_json(usage));
        }
        JsonValue::Object(object)
    }

    fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("message must be an object".to_string()))?;
        let role = match object
            .get("role")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| SessionError::Format("missing role".to_string()))?
        {
            "system" => MessageRole::System,
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            "tool" => MessageRole::Tool,
            other => {
                return Err(SessionError::Format(format!(
                    "unsupported message role: {other}"
                )))
            }
        };
        let blocks = object
            .get("blocks")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| SessionError::Format("missing blocks".to_string()))?
            .iter()
            .map(ContentBlock::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let usage = object.get("usage").map(usage_from_json).transpose()?;
        Ok(Self {
            role,
            blocks,
            usage,
        })
    }
}

impl ContentBlock {
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let mut object = BTreeMap::new();
        match self {
            Self::Text { text } => {
                object.insert("type".to_string(), JsonValue::String("text".to_string()));
                object.insert("text".to_string(), JsonValue::String(text.clone()));
            }
            Self::ToolUse { id, name, input } => {
                object.insert(
                    "type".to_string(),
                    JsonValue::String("tool_use".to_string()),
                );
                object.insert("id".to_string(), JsonValue::String(id.clone()));
                object.insert("name".to_string(), JsonValue::String(name.clone()));
                object.insert("input".to_string(), JsonValue::String(input.clone()));
            }
            Self::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => {
                object.insert(
                    "type".to_string(),
                    JsonValue::String("tool_result".to_string()),
                );
                object.insert(
                    "tool_use_id".to_string(),
                    JsonValue::String(tool_use_id.clone()),
                );
                object.insert(
                    "tool_name".to_string(),
                    JsonValue::String(tool_name.clone()),
                );
                object.insert("output".to_string(), JsonValue::String(output.clone()));
                object.insert("is_error".to_string(), JsonValue::Bool(*is_error));
            }
            Self::UserInputRequest {
                request_id,
                prompt,
                options,
                allow_freeform,
            } => {
                object.insert(
                    "type".to_string(),
                    JsonValue::String("user_input_request".to_string()),
                );
                object.insert(
                    "request_id".to_string(),
                    JsonValue::String(request_id.clone()),
                );
                object.insert("prompt".to_string(), JsonValue::String(prompt.clone()));
                object.insert(
                    "options".to_string(),
                    JsonValue::Array(
                        options
                            .iter()
                            .cloned()
                            .map(JsonValue::String)
                            .collect::<Vec<_>>(),
                    ),
                );
                object.insert(
                    "allow_freeform".to_string(),
                    JsonValue::Bool(*allow_freeform),
                );
            }
            Self::UserInputResponse {
                request_id,
                content,
                selected_option,
            } => {
                object.insert(
                    "type".to_string(),
                    JsonValue::String("user_input_response".to_string()),
                );
                object.insert(
                    "request_id".to_string(),
                    JsonValue::String(request_id.clone()),
                );
                object.insert("content".to_string(), JsonValue::String(content.clone()));
                if let Some(selected_option) = selected_option {
                    object.insert(
                        "selected_option".to_string(),
                        JsonValue::String(selected_option.clone()),
                    );
                }
            }
        }
        JsonValue::Object(object)
    }

    fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("block must be an object".to_string()))?;
        match object
            .get("type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| SessionError::Format("missing block type".to_string()))?
        {
            "text" => Ok(Self::Text {
                text: required_string(object, "text")?,
            }),
            "tool_use" => Ok(Self::ToolUse {
                id: required_string(object, "id")?,
                name: required_string(object, "name")?,
                input: required_string(object, "input")?,
            }),
            "tool_result" => Ok(Self::ToolResult {
                tool_use_id: required_string(object, "tool_use_id")?,
                tool_name: required_string(object, "tool_name")?,
                output: required_string(object, "output")?,
                is_error: object
                    .get("is_error")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| SessionError::Format("missing is_error".to_string()))?,
            }),
            "user_input_request" => Ok(Self::UserInputRequest {
                request_id: required_string(object, "request_id")?,
                prompt: required_string(object, "prompt")?,
                options: required_string_array(object, "options")?,
                allow_freeform: required_bool(object, "allow_freeform")?,
            }),
            "user_input_response" => Ok(Self::UserInputResponse {
                request_id: required_string(object, "request_id")?,
                content: required_string(object, "content")?,
                selected_option: optional_string(object, "selected_option")?,
            }),
            other => Err(SessionError::Format(format!(
                "unsupported block type: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUserInputRequest {
    pub request_id: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub allow_freeform: bool,
}

fn usage_to_json(usage: TokenUsage) -> JsonValue {
    let mut object = BTreeMap::new();
    object.insert(
        "input_tokens".to_string(),
        JsonValue::Number(i64::from(usage.input_tokens)),
    );
    object.insert(
        "output_tokens".to_string(),
        JsonValue::Number(i64::from(usage.output_tokens)),
    );
    object.insert(
        "cache_creation_input_tokens".to_string(),
        JsonValue::Number(i64::from(usage.cache_creation_input_tokens)),
    );
    object.insert(
        "cache_read_input_tokens".to_string(),
        JsonValue::Number(i64::from(usage.cache_read_input_tokens)),
    );
    JsonValue::Object(object)
}

fn usage_from_json(value: &JsonValue) -> Result<TokenUsage, SessionError> {
    let object = value
        .as_object()
        .ok_or_else(|| SessionError::Format("usage must be an object".to_string()))?;
    Ok(TokenUsage {
        input_tokens: required_u32(object, "input_tokens")?,
        output_tokens: required_u32(object, "output_tokens")?,
        cache_creation_input_tokens: required_u32(object, "cache_creation_input_tokens")?,
        cache_read_input_tokens: required_u32(object, "cache_read_input_tokens")?,
    })
}

fn telemetry_to_json(telemetry: SessionTelemetry) -> JsonValue {
    let mut object = BTreeMap::new();
    object.insert(
        "compacted_usage".to_string(),
        usage_to_json(telemetry.compacted_usage),
    );
    object.insert(
        "compacted_turns".to_string(),
        JsonValue::Number(i64::from(telemetry.compacted_turns)),
    );
    object.insert(
        "accounting_status".to_string(),
        JsonValue::String(
            match telemetry.accounting_status {
                SessionAccountingStatus::Complete => "complete",
                SessionAccountingStatus::PartialLegacyCompaction => "partial_legacy_compaction",
            }
            .to_string(),
        ),
    );
    JsonValue::Object(object)
}

fn telemetry_from_json(value: &JsonValue) -> Result<SessionTelemetry, SessionError> {
    let object = value
        .as_object()
        .ok_or_else(|| SessionError::Format("telemetry must be an object".to_string()))?;
    let accounting_status = match object
        .get("accounting_status")
        .and_then(JsonValue::as_str)
        .unwrap_or("complete")
    {
        "complete" => SessionAccountingStatus::Complete,
        "partial_legacy_compaction" => SessionAccountingStatus::PartialLegacyCompaction,
        other => {
            return Err(SessionError::Format(format!(
                "unsupported accounting_status: {other}"
            )))
        }
    };
    Ok(SessionTelemetry {
        compacted_usage: object
            .get("compacted_usage")
            .map(usage_from_json)
            .transpose()?
            .unwrap_or_default(),
        compacted_turns: object
            .get("compacted_turns")
            .map(|_| required_u32(object, "compacted_turns"))
            .transpose()?
            .unwrap_or(0),
        accounting_status,
    })
}

fn required_string(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<String, SessionError> {
    object
        .get(key)
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))
}

fn required_u32(object: &BTreeMap<String, JsonValue>, key: &str) -> Result<u32, SessionError> {
    let value = object
        .get(key)
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))?;
    u32::try_from(value).map_err(|_| SessionError::Format(format!("{key} out of range")))
}

fn required_bool(object: &BTreeMap<String, JsonValue>, key: &str) -> Result<bool, SessionError> {
    object
        .get(key)
        .and_then(JsonValue::as_bool)
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))
}

fn optional_string(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<Option<String>, SessionError> {
    match object.get(key) {
        Some(value) => value
            .as_str()
            .map(ToOwned::to_owned)
            .map(Some)
            .ok_or_else(|| SessionError::Format(format!("invalid {key}"))),
        None => Ok(None),
    }
}

fn required_string_array(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<Vec<String>, SessionError> {
    object
        .get(key)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| SessionError::Format(format!("invalid {key} item")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ContentBlock, ConversationMessage, MessageRole, Session, SessionAccountingStatus,
        SessionTelemetry,
    };
    use crate::json::JsonValue;
    use crate::usage::TokenUsage;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn persists_and_restores_session_json() {
        let mut session = Session::new();
        session.telemetry = Some(SessionTelemetry {
            compacted_usage: TokenUsage {
                input_tokens: 3,
                output_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 2,
            },
            compacted_turns: 1,
            accounting_status: SessionAccountingStatus::Complete,
        });
        session
            .messages
            .push(ConversationMessage::user_text("hello"));
        session
            .messages
            .push(ConversationMessage::assistant_with_usage(
                vec![
                    ContentBlock::Text {
                        text: "thinking".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "tool-1".to_string(),
                        name: "bash".to_string(),
                        input: "echo hi".to_string(),
                    },
                ],
                Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 4,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 2,
                }),
            ));
        session.messages.push(ConversationMessage::tool_result(
            "tool-1", "bash", "hi", false,
        ));

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("runtime-session-{nanos}.json"));
        session.save_to_path(&path).expect("session should save");
        let restored = Session::load_from_path(&path).expect("session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        assert_eq!(restored, session);
        assert_eq!(restored.messages[2].role, MessageRole::Tool);
        assert_eq!(
            restored.messages[1].usage.expect("usage").total_tokens(),
            17
        );
        assert_eq!(
            restored.telemetry,
            Some(SessionTelemetry {
                compacted_usage: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 2,
                },
                compacted_turns: 1,
                accounting_status: SessionAccountingStatus::Complete,
            })
        );
    }

    #[test]
    fn persists_and_detects_pending_user_input_request() {
        let mut session = Session::new();
        session.messages.push(ConversationMessage::assistant(vec![
            ContentBlock::UserInputRequest {
                request_id: "req-1".to_string(),
                prompt: "Choose a path".to_string(),
                options: vec!["keep".to_string(), "redo".to_string()],
                allow_freeform: false,
            },
        ]));

        let pending = session
            .pending_user_input_request()
            .expect("pending request should exist");
        assert_eq!(pending.request_id, "req-1");
        assert_eq!(pending.options, vec!["keep", "redo"]);
        assert!(!pending.allow_freeform);
    }

    #[test]
    fn user_input_response_clears_pending_request() {
        let mut session = Session::new();
        session.messages.push(ConversationMessage::assistant(vec![
            ContentBlock::UserInputRequest {
                request_id: "req-1".to_string(),
                prompt: "Choose a path".to_string(),
                options: vec!["keep".to_string(), "redo".to_string()],
                allow_freeform: false,
            },
        ]));
        session
            .messages
            .push(ConversationMessage::user_input_response(
                "req-1",
                "keep",
                Some("keep".to_string()),
            ));

        assert!(session.pending_user_input_request().is_none());
    }

    #[test]
    fn loads_legacy_version_one_session_without_user_input_blocks() {
        let session = Session::from_json(
            &JsonValue::parse(
                r#"{
  "version": 1,
  "messages": [
    {
      "role": "user",
      "blocks": [{ "type": "text", "text": "hello" }]
    },
    {
      "role": "assistant",
      "blocks": [{ "type": "text", "text": "world" }]
    }
  ]
}"#,
            )
            .expect("legacy session json should parse"),
        )
        .expect("legacy session should load");

        assert_eq!(session.version, 1);
        assert_eq!(session.messages.len(), 2);
        assert!(session.telemetry.is_none());
        assert!(session.pending_user_input_request().is_none());
    }

    #[test]
    fn loads_session_telemetry_with_partial_accounting_status() {
        let session = Session::from_json(
            &JsonValue::parse(
                r#"{
  "version": 1,
  "messages": [],
  "telemetry": {
    "compacted_usage": {
      "input_tokens": 8,
      "output_tokens": 3,
      "cache_creation_input_tokens": 1,
      "cache_read_input_tokens": 2
    },
    "compacted_turns": 2,
    "accounting_status": "partial_legacy_compaction"
  }
}"#,
            )
            .expect("telemetry session json should parse"),
        )
        .expect("telemetry session should load");

        assert_eq!(
            session.telemetry,
            Some(SessionTelemetry {
                compacted_usage: TokenUsage {
                    input_tokens: 8,
                    output_tokens: 3,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 2,
                },
                compacted_turns: 2,
                accounting_status: SessionAccountingStatus::PartialLegacyCompaction,
            })
        );
    }
}
