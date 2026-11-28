//! JSON-RPC 2.0 framing for MCP's Streamable HTTP transport, written by hand
//! with serde: one message per POST, one JSON answer, no batches (MCP removed
//! them in 2025-06-18), no server-to-client stream.

use serde_json::{json, Value};

/// The newest MCP protocol version this server speaks.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Every MCP protocol version this server speaks, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// The version to answer `initialize` with: the client's when this server
/// speaks it, otherwise the newest this server speaks (the client then
/// decides whether to go on).
pub fn negotiate(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| SUPPORTED_PROTOCOL_VERSIONS.iter().find(|v| **v == r))
        .copied()
        .unwrap_or(LATEST_PROTOCOL_VERSION)
}

/// One message read from a POST body.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// A request: it gets an answer with the same `id`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification: no `id`, no answer.
    Notification { method: String },
}

/// Read one JSON-RPC message, or the error answer to send instead.
pub fn parse(body: &[u8]) -> Result<Incoming, Value> {
    let msg: Value = serde_json::from_slice(body)
        .map_err(|e| error(Value::Null, PARSE_ERROR, &format!("not JSON: {e}")))?;
    let Value::Object(obj) = msg else {
        let why = if msg.is_array() {
            "a batch is not accepted: send one message per request"
        } else {
            "a message is a JSON object"
        };
        return Err(error(Value::Null, INVALID_REQUEST, why));
    };
    // The id, when there is a usable one, goes on any error answer.
    let id = match obj.get("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_))) => Some(v.clone()),
        Some(_) => {
            return Err(error(
                Value::Null,
                INVALID_REQUEST,
                "`id` is a string or a number",
            ))
        }
    };
    let err_id = id.clone().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(error(err_id, INVALID_REQUEST, "`jsonrpc` must be \"2.0\""));
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Err(error(err_id, INVALID_REQUEST, "`method` is a string"));
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if !(params.is_null() || params.is_object()) {
        return Err(error(err_id, INVALID_PARAMS, "`params` is an object"));
    }
    Ok(match id {
        Some(id) => Incoming::Request {
            id,
            method: method.to_string(),
            params,
        },
        None => Incoming::Notification {
            method: method.to_string(),
        },
    })
}

/// A success answer.
pub fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// An error answer.
pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(v: &Value) -> i64 {
        v["error"]["code"].as_i64().unwrap()
    }

    #[test]
    fn negotiation_echoes_a_known_version_and_offers_the_newest_otherwise() {
        assert_eq!(negotiate(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate(Some("2025-03-26")), "2025-03-26");
        assert_eq!(negotiate(Some("2024-11-05")), LATEST_PROTOCOL_VERSION);
        assert_eq!(negotiate(None), LATEST_PROTOCOL_VERSION);
    }

    #[test]
    fn a_request_and_a_notification_are_told_apart_by_the_id() {
        let r = parse(br#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#).unwrap();
        assert_eq!(
            r,
            Incoming::Request {
                id: json!(7),
                method: "ping".into(),
                params: Value::Null
            }
        );
        let n = parse(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert_eq!(
            n,
            Incoming::Notification {
                method: "notifications/initialized".into()
            }
        );
    }

    #[test]
    fn malformed_messages_get_the_matching_error_code() {
        assert_eq!(code(&parse(b"{not json").unwrap_err()), PARSE_ERROR);
        assert_eq!(
            code(&parse(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).unwrap_err()),
            INVALID_REQUEST
        );
        assert_eq!(code(&parse(br#""ping""#).unwrap_err()), INVALID_REQUEST);
        assert_eq!(
            code(&parse(br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).unwrap_err()),
            INVALID_REQUEST
        );
        assert_eq!(
            code(&parse(br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).unwrap_err()),
            INVALID_REQUEST
        );
        assert_eq!(
            code(&parse(br#"{"jsonrpc":"2.0","id":1}"#).unwrap_err()),
            INVALID_REQUEST
        );
        let e = parse(br#"{"jsonrpc":"2.0","id":"a","method":"x","params":[1]}"#).unwrap_err();
        assert_eq!(code(&e), INVALID_PARAMS);
        assert_eq!(e["id"], json!("a"));
    }
}
