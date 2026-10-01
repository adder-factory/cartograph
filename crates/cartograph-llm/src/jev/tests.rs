use std::{
    io::{Read as _, Write as _},
    net::TcpListener,
    thread,
};

use super::*;

#[test]
fn missing_credential_reports_only_the_validated_variable_name() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
    let variable = format!("CARTOGRAPH_JEV_MISSING_TEST_{}", std::process::id());
    assert!(std::env::var_os(&variable).is_none());
    let input = crate::ProjectLlmTierInput::jev(&variable)
        .unwrap_or_else(|error| panic!("valid tier: {error}"));
    crate::write_project_llm_tiers(root.path(), &[input])
        .unwrap_or_else(|error| panic!("write config: {error}"));
    let Err(error) = JevSettings::try_from_project(root.path()) else {
        panic!("missing key was accepted");
    };
    assert_eq!(
        serde_json::to_value(&error).unwrap_or_else(|error| panic!("error code: {error}")),
        "credential_missing"
    );
    assert_eq!(
        error.to_string(),
        format!("Cartograph Jev environment variable {variable} is not set in this process")
    );
}

fn questions() -> BTreeMap<String, JevQuestion> {
    BTreeMap::from([
        (
            "action".to_owned(),
            JevQuestion::Choice {
                instructions: "Choose the next action.".to_owned(),
                criteria: BTreeMap::from([
                    ("read".to_owned(), "Inspect source".to_owned()),
                    ("stop".to_owned(), "Finish".to_owned()),
                ]),
            },
        ),
        (
            "sufficient".to_owned(),
            JevQuestion::Noul {
                instructions: "Is the available source sufficient?".to_owned(),
                criteria: None,
            },
        ),
    ])
}

fn valid_response() -> Value {
    serde_json::json!({"model":JEV_MODEL,"answers":{"action":{"type":"choice","choice":"read","probabilities":{"read":0.9,"stop":0.1},"confidence":0.8},"sufficient":{"type":"noul","noul":0.2}}})
}

#[test]
fn parallel_answer_validation_rejects_incomplete_or_foreign_decisions() {
    let q = questions();
    assert!(
        decode_response(
            &serde_json::to_vec(&valid_response()).unwrap_or_default(),
            &q
        )
        .is_ok()
    );
    let mut cases = Vec::new();
    let mut response = valid_response();
    response["model"] = "unreviewed-model".into();
    cases.push(response);
    let mut response = valid_response();
    response["answers"]["action"]["choice"] = "delete".into();
    cases.push(response);
    let mut response = valid_response();
    response["answers"]["action"]["confidence"] = 1.2.into();
    cases.push(response);
    let mut response = valid_response();
    response["answers"]["sufficient"]["noul"] = (-0.1).into();
    cases.push(response);
    let mut response = valid_response();
    response["answers"]["sufficient"] = serde_json::json!({"type":"choice","choice":"read","probabilities":{"read":1.0},"confidence":1.0});
    cases.push(response);
    let mut response = valid_response();
    response["answers"]["action"]["probabilities"]["stop"] = 0.95.into();
    cases.push(response);
    let mut response = valid_response();
    response["answers"]["extra"] = serde_json::json!({"type":"noul","noul":1.0});
    cases.push(response);
    let mut response = valid_response();
    response["answers"]
        .as_object_mut()
        .unwrap_or_else(|| panic!("fixture object"))
        .remove("sufficient");
    cases.push(response);
    for response in cases {
        assert!(matches!(
            decode_response(&serde_json::to_vec(&response).unwrap_or_default(), &q),
            Err(JevError::InvalidResponse)
        ));
    }
    let duplicate = format!(
        r#"{{"model":"{JEV_MODEL}","answers":{{"action":{{"type":"choice","choice":"read","probabilities":{{"read":0.9,"read":0.9,"stop":0.1}},"confidence":0.8}},"sufficient":{{"type":"noul","noul":0.2}}}}}}"#
    );
    assert!(matches!(
        decode_response(duplicate.as_bytes(), &q),
        Err(JevError::InvalidResponse)
    ));
    let duplicate = format!(
        r#"{{"model":"{JEV_MODEL}","answers":{{"sufficient":{{"type":"noul","noul":0.2}},"sufficient":{{"type":"noul","noul":0.8}}}}}}"#
    );
    assert!(matches!(
        decode_response(duplicate.as_bytes(), &q),
        Err(JevError::InvalidResponse)
    ));
}

#[test]
fn request_bounds_apply_before_transport() {
    assert!(matches!(
        encode_request(&Value::Null, &BTreeMap::new()),
        Err(JevError::RequestLimit)
    ));
    assert!(matches!(
        encode_request(
            &Value::String("s".repeat(MAXIMUM_STATE_BYTES)),
            &questions()
        ),
        Err(JevError::RequestLimit)
    ));
    let q = BTreeMap::from([(
        "one".to_owned(),
        JevQuestion::Choice {
            instructions: "select".to_owned(),
            criteria: BTreeMap::from([("only".to_owned(), "only".to_owned())]),
        },
    )]);
    assert!(matches!(
        encode_request(&Value::Null, &q),
        Err(JevError::RequestLimit)
    ));
    let body = encode_request(&serde_json::json!({"task":"find code"}), &questions())
        .unwrap_or_else(|e| panic!("encode: {e}"));
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|e| panic!("JSON: {e}"));
    assert_eq!(
        value["questions"].as_object().map(serde_json::Map::len),
        Some(2)
    );
    assert_eq!(value["model"], JEV_MODEL);
}

fn fixture(status: &str, body: &str, headers: &str) -> (JevClient, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("listen: {e}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("address: {e}"));
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    );
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap_or_else(|e| panic!("accept: {e}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap_or_else(|e| panic!("timeout: {e}"));
        let mut received = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let read = stream
                .read(&mut chunk)
                .unwrap_or_else(|e| panic!("read: {e}"));
            assert!(read > 0 && received.len() < MAXIMUM_REQUEST_BYTES);
            received.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&received);
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|n| n.parse::<usize>().ok())
                    })
                    .unwrap_or_default();
                if received.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let _ = stream.write_all(response.as_bytes());
        String::from_utf8(received).unwrap_or_else(|e| panic!("UTF-8: {e}"))
    });
    let settings = JevSettings {
        endpoint: Url::parse(&format!("http://{address}/v1/systemone"))
            .unwrap_or_else(|e| panic!("URL: {e}")),
        api_key: SecretString::from("private-fixture-key".to_owned()),
        timeout: Duration::from_secs(2),
        features: BTreeSet::from([JevFeature::Explore]),
    };
    assert!(!format!("{settings:?}").contains("private-fixture"));
    (
        JevClient::new(settings).unwrap_or_else(|e| panic!("client: {e}")),
        handle,
    )
}

#[tokio::test]
async fn one_http_request_contains_all_parallel_questions_and_bearer_credential() {
    let (client, server) = fixture("200 OK", &valid_response().to_string(), "");
    let result = client
        .decide(&serde_json::json!({"task":"find code"}), &questions())
        .await
        .unwrap_or_else(|e| panic!("decision: {e}"));
    assert_eq!(result.answers.len(), 2);
    let request = server.join().unwrap_or_else(|_| panic!("fixture server"));
    assert!(request.starts_with("POST /v1/systemone HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer private-fixture-key")
    );
    let (_, body) = request
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("HTTP separator"));
    let body: Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("request JSON: {e}"));
    assert_eq!(
        body["questions"].as_object().map(serde_json::Map::len),
        Some(2)
    );
    assert!(!body.to_string().contains("private-fixture-key"));
}

#[tokio::test]
async fn rejected_credentials_capacity_and_redirects_are_redacted() {
    for (status, expected) in [
        ("401 Unauthorized", JevError::AuthenticationFailed),
        ("429 Too Many Requests", JevError::RateLimited),
        ("529 Overloaded", JevError::RateLimited),
        ("422 Invalid", JevError::BackendRejected),
        ("503 Service Unavailable", JevError::EndpointUnavailable),
    ] {
        let (client, server) = fixture(status, "secret-provider-body", "");
        let result = client.decide(&Value::Null, &questions()).await;
        assert!(matches!(&result, Err(error) if error == &expected));
        assert!(!format!("{result:?}").contains("secret-provider-body"));
        server.join().unwrap_or_else(|_| panic!("fixture server"));
    }
    let target =
        TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("redirect listener: {e}"));
    target
        .set_nonblocking(true)
        .unwrap_or_else(|e| panic!("nonblocking: {e}"));
    let address = target
        .local_addr()
        .unwrap_or_else(|e| panic!("redirect address: {e}"));
    let (client, server) = fixture(
        "302 Found",
        "",
        &format!("Location: http://{address}/must-not-receive-key\r\n"),
    );
    assert!(matches!(
        client.decide(&Value::Null, &questions()).await,
        Err(JevError::BackendRejected)
    ));
    assert!(matches!(target.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    server.join().unwrap_or_else(|_| panic!("fixture server"));
}

#[tokio::test]
async fn oversized_and_malformed_provider_bodies_cannot_control_retrieval() {
    for (body, expected) in [
        (
            "x".repeat(MAXIMUM_RESPONSE_BYTES + 1),
            JevError::ResponseLimit,
        ),
        ("not-json".to_owned(), JevError::InvalidResponse),
    ] {
        let (client, server) = fixture("200 OK", &body, "");
        assert!(
            matches!(client.decide(&Value::Null, &questions()).await, Err(error) if error == expected)
        );
        server.join().unwrap_or_else(|_| panic!("fixture server"));
    }
}

#[test]
fn decision_settings_reject_unreviewed_models_endpoints_and_excessive_deadlines() {
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("project: {e}"));
    std::fs::create_dir(root.path().join(".cartograph")).unwrap_or_else(|e| panic!("marker: {e}"));
    let valid = serde_json::json!({"version":2,"llm":{"enabled":true,"decisionLlm":{"provider":"typesafe","model":JEV_MODEL,"endpoint":JEV_ENDPOINT,"apiKey":"synthetic-fixture-key"}}});
    for (field, value) in [
        ("model", Value::from("latest")),
        (
            "endpoint",
            Value::from("https://untrusted.example/v1/systemone"),
        ),
        ("timeoutMs", Value::from(30001)),
    ] {
        let mut config = valid.clone();
        config["llm"]["decisionLlm"][field] = value;
        std::fs::write(
            root.path().join(".cartograph/config.json"),
            config.to_string(),
        )
        .unwrap_or_else(|e| panic!("config: {e}"));
        assert!(matches!(
            JevSettings::try_from_project(root.path()),
            Err(JevError::ConfigurationUnavailable)
        ));
    }
    std::fs::write(
        root.path().join(".cartograph/config.json"),
        valid.to_string(),
    )
    .unwrap_or_else(|e| panic!("config: {e}"));
    let settings = JevSettings::try_from_project(root.path())
        .unwrap_or_else(|e| panic!("settings: {e}"))
        .unwrap_or_else(|| panic!("missing tier"));
    assert_eq!(settings.timeout, DEFAULT_TIMEOUT);
    assert!(!format!("{settings:?}").contains("synthetic-fixture-key"));
}

#[test]
fn decision_config_is_opt_in_and_never_falls_back_to_chat() {
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("project: {e}"));
    std::fs::create_dir(root.path().join(".cartograph")).unwrap_or_else(|e| panic!("marker: {e}"));
    std::fs::write(root.path().join(".cartograph/config.json"), r#"{"version":2,"llm":{"enabled":true,"summarizeLlm":{"provider":"openai-compat","endpoint":"https://example.test","model":"chat"}}}"#).unwrap_or_else(|e| panic!("config: {e}"));
    assert!(
        JevSettings::try_from_project(root.path())
            .unwrap_or_else(|e| panic!("load: {e}"))
            .is_none()
    );
    let input = crate::ProjectLlmTierInput::jev("CARTOGRAPH_MISSING_JEV_FIXTURE_KEY")
        .unwrap_or_else(|e| panic!("input: {e}"));
    crate::write_project_llm_tiers(root.path(), &[input]).unwrap_or_else(|e| panic!("write: {e}"));
    assert!(matches!(
        JevSettings::try_from_project(root.path()),
        Err(JevError::CredentialMissing { environment_variable })
            if environment_variable == "CARTOGRAPH_MISSING_JEV_FIXTURE_KEY"
    ));
    let text = std::fs::read_to_string(root.path().join(".cartograph/config.json"))
        .unwrap_or_else(|e| panic!("read: {e}"));
    let config: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("JSON: {e}"));
    assert_eq!(config["llm"]["decisionLlm"]["provider"], "typesafe");
    assert_eq!(
        config["llm"]["decisionLlm"]["apiKeyEnv"],
        "CARTOGRAPH_MISSING_JEV_FIXTURE_KEY"
    );
    assert!(config["llm"]["decisionLlm"].get("apiKey").is_none());
    assert_eq!(config["llm"]["summarizeLlm"]["model"], "chat");
}

#[tokio::test]
#[ignore = "requires an explicitly supplied TYPESAFE_AI_JEV_API_KEY for a real bounded provider request"]
async fn jev_live_parallel_decision() {
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("project: {e}"));
    std::fs::create_dir(root.path().join(".cartograph")).unwrap_or_else(|e| panic!("marker: {e}"));
    let input = crate::ProjectLlmTierInput::jev("TYPESAFE_AI_JEV_API_KEY")
        .unwrap_or_else(|e| panic!("input: {e}"));
    crate::write_project_llm_tiers(root.path(), &[input]).unwrap_or_else(|e| panic!("write: {e}"));
    let settings = JevSettings::try_from_project(root.path())
        .unwrap_or_else(|e| panic!("settings: {e}"))
        .unwrap_or_else(|| panic!("tier absent"));
    let client = JevClient::new(settings).unwrap_or_else(|e| panic!("client: {e}"));
    let result = client.decide(&serde_json::json!({"task":"Find the declaration of the parser. No source has been read."}), &questions()).await.unwrap_or_else(|e| panic!("live decision: {e}"));
    assert_eq!(result.model, JEV_MODEL);
    assert!(
        matches!(result.answers.get("action"), Some(JevAnswer::Choice { choice, .. }) if choice == "read")
    );
    assert!(
        matches!(result.answers.get("sufficient"), Some(JevAnswer::Noul { noul }) if *noul < 0.5)
    );
}

#[test]
fn noul_criteria_serialize_with_api_names_and_share_criterion_bounds() {
    let criteria = |holds: String| {
        BTreeMap::from([(
            "relevant".to_owned(),
            JevQuestion::Noul {
                instructions: "Does the candidate implement the behavior?".to_owned(),
                criteria: Some(NoulCriteria {
                    holds,
                    fails: "Only shares vocabulary with the task.".to_owned(),
                }),
            },
        )])
    };
    let encoded = encode_request(
        &serde_json::json!({"task": "x"}),
        &criteria("Its body decides the behavior.".to_owned()),
    )
    .unwrap_or_else(|error| panic!("bounded criteria: {error}"));
    let request: Value =
        serde_json::from_slice(&encoded).unwrap_or_else(|error| panic!("request json: {error}"));
    assert_eq!(
        request["questions"]["relevant"]["criteria"],
        serde_json::json!({"true": "Its body decides the behavior.", "false": "Only shares vocabulary with the task."})
    );
    for invalid in [String::new(), "x".repeat(2049), "nul\0".to_owned()] {
        assert_eq!(
            encode_request(&serde_json::json!({"task": "x"}), &criteria(invalid)),
            Err(JevError::RequestLimit)
        );
    }
    let without = BTreeMap::from([(
        "sufficient".to_owned(),
        JevQuestion::Noul {
            instructions: "Is it sufficient?".to_owned(),
            criteria: None,
        },
    )]);
    let encoded = encode_request(&serde_json::json!({"task": "x"}), &without)
        .unwrap_or_else(|error| panic!("criteria-free noul: {error}"));
    let request: Value =
        serde_json::from_slice(&encoded).unwrap_or_else(|error| panic!("request json: {error}"));
    assert!(request["questions"]["sufficient"].get("criteria").is_none());
}

#[test]
fn decision_features_default_to_exploration_and_ignore_unknown_names() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
    let variable = format!("CARTOGRAPH_JEV_FEATURE_TEST_{}", std::process::id());
    let write = |features: Option<Vec<&str>>| {
        let mut input = crate::ProjectLlmTierInput::jev(&variable)
            .unwrap_or_else(|error| panic!("tier: {error}"));
        if let Some(features) = features {
            input = input
                .with_decision_features(features.into_iter().map(str::to_owned).collect())
                .unwrap_or_else(|error| panic!("features: {error}"));
        }
        crate::write_project_llm_tiers(root.path(), &[input])
            .unwrap_or_else(|error| panic!("write config: {error}"));
    };
    write(None);
    assert!(jev_feature_enabled(root.path(), JevFeature::Explore));
    assert!(!jev_feature_enabled(root.path(), JevFeature::Context));
    write(Some(vec!["context", "future_surface"]));
    assert!(!jev_feature_enabled(root.path(), JevFeature::Explore));
    assert!(jev_feature_enabled(root.path(), JevFeature::Context));
    write(Some(Vec::new()));
    assert!(!jev_feature_enabled(root.path(), JevFeature::Explore));
    assert!(!jev_feature_enabled(root.path(), JevFeature::Context));
    assert!(
        crate::ProjectLlmTierInput::jev(&variable)
            .and_then(|input| input.with_decision_features(vec!["Context".to_owned()]))
            .is_err(),
        "feature names are lowercase identifiers"
    );
}
