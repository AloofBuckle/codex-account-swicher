use crate::{AuthIdentity, CasError, Result};
use base64::Engine as _;
use chrono::DateTime;
use serde_json::Value;

pub fn validate_auth(bytes: &[u8]) -> Result<AuthIdentity> {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let root: Value = serde_json::from_slice(bytes)?;
    if !root.is_object() {
        return Err(CasError::InvalidAuth(
            "auth.json must contain a JSON object".into(),
        ));
    }

    let payload = root
        .get("tokens")
        .and_then(|v| v.get("id_token"))
        .and_then(Value::as_str)
        .and_then(decode_jwt_payload);

    let email = payload
        .as_ref()
        .and_then(|value| find_string(value, &["email"]))
        .or_else(|| find_string(&root, &["email"]));
    let account_id = payload
        .as_ref()
        .and_then(|value| find_string(value, &["chatgpt_account_id"]))
        .or_else(|| find_string(&root, &["chatgpt_account_id"]))
        .or_else(|| find_string(&root, &["account_id"]))
        .or_else(|| {
            payload
                .as_ref()
                .and_then(|value| find_string(value, &["account_id"]))
        });
    let user_id = payload
        .as_ref()
        .and_then(|value| find_string(value, &["chatgpt_user_id"]))
        .or_else(|| find_string(&root, &["chatgpt_user_id"]))
        .or_else(|| {
            payload
                .as_ref()
                .and_then(|value| find_string(value, &["user_id"]))
        })
        .or_else(|| find_string(&root, &["user_id"]))
        .or_else(|| {
            payload
                .as_ref()
                .and_then(|value| find_string(value, &["sub"]))
        })
        .or_else(|| find_string(&root, &["sub"]));

    Ok(AuthIdentity {
        email,
        account_id,
        user_id,
    })
}

pub(crate) fn auth_subject(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let root: Value = serde_json::from_slice(bytes).ok()?;
    let payload = root
        .get("tokens")
        .and_then(|v| v.get("id_token"))
        .and_then(Value::as_str)
        .and_then(decode_jwt_payload);
    payload
        .as_ref()
        .and_then(|value| find_string(value, &["sub"]))
        .or_else(|| find_string(&root, &["sub"]))
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(s) = map.get(*key).and_then(Value::as_str) {
                    let s = s.trim();
                    if !s.is_empty() {
                        return Some(s.to_owned());
                    }
                }
            }
            for child in map.values() {
                if let Some(found) = find_string(child, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|v| find_string(v, keys)),
        _ => None,
    }
}

fn decode_jwt_payload(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn identities_match(a: &AuthIdentity, b: &AuthIdentity) -> bool {
    if a.account_id.is_some() || b.account_id.is_some() {
        let workspace_matches =
            matches!((&a.account_id, &b.account_id), (Some(a), Some(b)) if a == b);
        if !workspace_matches {
            return false;
        }
        if a.user_id.is_some() || b.user_id.is_some() {
            return matches!((&a.user_id, &b.user_id), (Some(a), Some(b)) if a == b);
        }
        return matches!((&a.email, &b.email), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
    }
    if a.user_id.is_some() || b.user_id.is_some() {
        return matches!((&a.user_id, &b.user_id), (Some(a), Some(b)) if a == b);
    }
    if let (Some(a), Some(b)) = (&a.email, &b.email) {
        return a.eq_ignore_ascii_case(b);
    }
    false
}

pub(crate) fn identity_can_enrich(recorded: &AuthIdentity, saved: &AuthIdentity) -> bool {
    if let (Some(left), Some(right)) = (&recorded.account_id, &saved.account_id)
        && left != right
    {
        return false;
    }
    if let (Some(left), Some(right)) = (&recorded.user_id, &saved.user_id)
        && left != right
    {
        return false;
    }
    if let (Some(left), Some(right)) = (&recorded.email, &saved.email)
        && !left.eq_ignore_ascii_case(right)
    {
        return false;
    }
    recorded.has_stable_field() && saved.has_stable_field()
}

pub(crate) fn auth_last_refresh_millis(bytes: &[u8]) -> Option<i64> {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let root: Value = serde_json::from_slice(bytes).ok()?;
    let value = root.get("last_refresh")?;
    if let Some(raw) = value.as_str() {
        return DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|timestamp| timestamp.timestamp_millis());
    }
    value.as_i64().map(|timestamp| {
        if timestamp.abs() < 10_000_000_000 {
            timestamp.saturating_mul(1000)
        } else {
            timestamp
        }
    })
}

pub(crate) fn auth_plan_type(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let root: Value = serde_json::from_slice(bytes).ok()?;
    let tokens = root.get("tokens")?;
    for token_name in ["id_token", "access_token"] {
        let Some(token) = tokens.get(token_name) else {
            continue;
        };
        let claims = match token {
            Value::String(raw) => decode_jwt_payload(raw),
            Value::Object(_) => Some(token.clone()),
            _ => None,
        };
        if let Some(claims) = claims
            && let Some(plan) = find_string(&claims, &["chatgpt_plan_type"])
        {
            return Some(normalize_plan_type(&plan));
        }
    }
    None
}

fn normalize_plan_type(plan: &str) -> String {
    let normalized = plan.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "team" => "business".into(),
        _ => normalized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_identity() {
        let data = br#"{
            "auth_mode":"chatgpt",
            "tokens":{"account_id":"acc_123","id_token":{"email":"a@example.com","chatgpt_user_id":"user_1"}}
        }"#;
        let id = validate_auth(data).unwrap();
        assert_eq!(id.email.as_deref(), Some("a@example.com"));
        assert_eq!(id.account_id.as_deref(), Some("acc_123"));
        assert_eq!(id.user_id.as_deref(), Some("user_1"));
    }

    #[test]
    fn chatgpt_user_id_beats_jwt_subject() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            br#"{"sub":"auth0|legacy","email":"a@example.com","https://api.openai.com/auth":{"chatgpt_account_id":"workspace-1","chatgpt_user_id":"user-official"}}"#,
        );
        let auth = format!(
            r#"{{"tokens":{{"account_id":"workspace-1","id_token":"header.{payload}.sig"}}}}"#
        );
        let identity = validate_auth(auth.as_bytes()).unwrap();
        assert_eq!(identity.user_id.as_deref(), Some("user-official"));
        assert_eq!(
            auth_subject(auth.as_bytes()).as_deref(),
            Some("auth0|legacy")
        );
    }

    #[test]
    fn accepts_utf8_bom() {
        let mut data = vec![0xef, 0xbb, 0xbf];
        data.extend_from_slice(br#"{"email":"bom@example.com"}"#);
        let id = validate_auth(&data).unwrap();
        assert_eq!(id.email.as_deref(), Some("bom@example.com"));
    }

    #[test]
    fn account_id_is_authoritative_across_workspaces() {
        let personal = AuthIdentity {
            email: Some("same@example.com".into()),
            account_id: Some("workspace-personal".into()),
            user_id: Some("user-1".into()),
        };
        let business = AuthIdentity {
            email: Some("same@example.com".into()),
            account_id: Some("workspace-business".into()),
            user_id: Some("user-1".into()),
        };
        assert!(!identities_match(&personal, &business));
    }

    #[test]
    fn different_users_in_same_workspace_are_different_accounts() {
        let gmail = AuthIdentity {
            email: Some("gmail@example.com".into()),
            account_id: Some("workspace-shared".into()),
            user_id: Some("user-gmail".into()),
        };
        let outlook = AuthIdentity {
            email: Some("outlook@example.com".into()),
            account_id: Some("workspace-shared".into()),
            user_id: Some("user-outlook".into()),
        };
        assert!(!identities_match(&gmail, &outlook));
    }

    #[test]
    fn same_user_in_same_workspace_is_the_same_account() {
        let first = AuthIdentity {
            email: Some("old@example.com".into()),
            account_id: Some("workspace-shared".into()),
            user_id: Some("user-one".into()),
        };
        let refreshed = AuthIdentity {
            email: Some("new@example.com".into()),
            account_id: Some("workspace-shared".into()),
            user_id: Some("user-one".into()),
        };
        assert!(identities_match(&first, &refreshed));
    }

    #[test]
    fn weaker_fields_are_used_only_when_stronger_id_is_missing() {
        let known = AuthIdentity {
            email: Some("same@example.com".into()),
            account_id: Some("workspace-personal".into()),
            user_id: Some("user-1".into()),
        };
        let legacy = AuthIdentity {
            email: Some("same@example.com".into()),
            account_id: None,
            user_id: Some("user-1".into()),
        };
        assert!(!identities_match(&known, &legacy));
        assert!(identity_can_enrich(&legacy, &known));
    }

    #[test]
    fn extracts_and_normalizes_plan_type_from_token_claims() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_plan_type":"team"}}"#);
        let auth = format!(
            r#"{{"tokens":{{"id_token":"header.{payload}.sig"}},"last_refresh":"2026-10-02T00:00:00Z"}}"#
        );
        assert_eq!(auth_plan_type(auth.as_bytes()).as_deref(), Some("business"));
    }
}
