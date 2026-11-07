//! Vercel AI Gateway Decisions client (`POST /v1/decisions`).
//!
//! `build_request`, `parse_response` and `parse_error` are pure. `send` is the
//! only function that touches the network and is blocking: the daemon runs it
//! inside a background job, never on the poll thread. Errors carry only a
//! [`Fallback`] label. Request URLs, response bodies and the API key are never
//! returned, logged or stored, because ureq transport errors embed the URL.

use std::io;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Answer, DecisionResponse, Fallback, Question};

/// The Gateway accepts more, but the limit is unverified; stay well below it.
pub const MAX_QUESTIONS: usize = 16;

fn question_json(question: &Question) -> Value {
    match question {
        Question::Predicate { name, instructions } => json!({
            "type": "predicate",
            "name": name,
            "instructions": instructions,
        }),
        Question::Choice {
            name,
            instructions,
            choices,
        } => {
            let choices: Vec<Value> = choices
                .iter()
                .map(|(value, description)| match description {
                    Some(description) => json!({"value": value, "description": description}),
                    None => json!({"value": value}),
                })
                .collect();
            json!({
                "type": "choice",
                "name": name,
                "instructions": instructions,
                "choices": choices,
            })
        }
        Question::Score {
            name,
            instructions,
            levels,
        } => {
            let levels: Vec<Value> = levels.iter().map(|label| json!({"label": label})).collect();
            json!({
                "type": "score",
                "name": name,
                "instructions": instructions,
                "levels": levels,
            })
        }
    }
}

/// Build the request body. At most [`MAX_QUESTIONS`] questions are included.
pub fn build_request(
    model: &str,
    input: &str,
    questions: &[Question],
    zero_data_retention: bool,
) -> Value {
    let questions: Vec<Value> = questions
        .iter()
        .take(MAX_QUESTIONS)
        .map(question_json)
        .collect();
    json!({
        "model": model,
        "input": input,
        "questions": questions,
        "providerOptions": {"gateway": {"zeroDataRetention": zero_data_retention}},
    })
}

/// Parse a successful response body. The cost is a decimal string in USD; a
/// missing or malformed cost counts as zero rather than failing the decision.
pub fn parse_response(body: &Value) -> Result<DecisionResponse, Fallback> {
    let raw_answers = body
        .get("answers")
        .and_then(Value::as_array)
        .ok_or(Fallback::InvalidAnswer)?;
    // An answer of a shape we do not understand becomes Unknown, so one odd
    // answer does not discard the others.
    let answers: Vec<Answer> = raw_answers
        .iter()
        .map(|raw| serde_json::from_value(raw.clone()).unwrap_or(Answer::Unknown))
        .collect();

    let cost_usd = body
        .pointer("/provider_metadata/gateway/cost")
        .and_then(Value::as_str)
        .and_then(|cost| cost.trim().parse::<f64>().ok())
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .unwrap_or(0.0);
    let input_tokens = body
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    Ok(DecisionResponse {
        answers,
        cost_usd,
        input_tokens,
    })
}

/// Map a non-success HTTP status to a fallback. 401, 402, 403, 429 and 5xx all
/// fall back without a retry; the body is never inspected or kept.
pub fn parse_error(status: u16) -> Fallback {
    Fallback::Http(status)
}

fn is_timeout(error: &ureq::Transport) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(io_error) = cause.downcast_ref::<io::Error>() {
            return matches!(
                io_error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            );
        }
        source = cause.source();
    }
    false
}

/// POST the request and parse the reply. `timeout` is an overall deadline.
pub fn send(
    endpoint: &str,
    key: &str,
    body: &Value,
    timeout: Duration,
) -> Result<DecisionResponse, Fallback> {
    let result = crate::team::http::agent()
        .post(endpoint)
        .timeout(timeout)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    match result {
        Ok(response) => {
            let json: Value = response.into_json().map_err(|error| {
                if error.kind() == io::ErrorKind::TimedOut {
                    Fallback::Timeout
                } else {
                    Fallback::InvalidAnswer
                }
            })?;
            parse_response(&json)
        }
        Err(ureq::Error::Status(code, _response)) => Err(parse_error(code)),
        Err(ureq::Error::Transport(transport)) if is_timeout(&transport) => Err(Fallback::Timeout),
        Err(ureq::Error::Transport(_)) => Err(Fallback::Http(0)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use super::*;

    fn predicate() -> Question {
        Question::Predicate {
            name: "done".into(),
            instructions: "Did it finish?".into(),
        }
    }

    fn choice() -> Question {
        Question::Choice {
            name: "kind".into(),
            instructions: "Classify".into(),
            choices: vec![
                ("actionable".into(), Some("needs action".into())),
                ("noise".into(), None),
            ],
        }
    }

    fn score() -> Question {
        Question::Score {
            name: "criterion_1".into(),
            instructions: "How well?".into(),
            levels: vec!["not met".into(), "partly met".into(), "met".into()],
        }
    }

    #[test]
    fn request_has_model_input_questions_and_retention() {
        let body = build_request(
            "typesafe-ai/jev-1.14.0",
            "state text",
            &[predicate(), choice(), score()],
            true,
        );
        assert_eq!(body["model"], "typesafe-ai/jev-1.14.0");
        assert_eq!(body["input"], "state text");
        assert_eq!(
            body["providerOptions"]["gateway"]["zeroDataRetention"],
            true
        );
        let questions = body["questions"].as_array().unwrap();
        assert_eq!(questions.len(), 3);
        assert_eq!(questions[0]["type"], "predicate");
        assert_eq!(questions[0]["name"], "done");
        assert_eq!(questions[1]["type"], "choice");
        assert_eq!(questions[1]["choices"][0]["value"], "actionable");
        assert_eq!(questions[1]["choices"][0]["description"], "needs action");
        assert!(questions[1]["choices"][1].get("description").is_none());
        assert_eq!(questions[2]["type"], "score");
        assert_eq!(questions[2]["levels"][2]["label"], "met");
    }

    #[test]
    fn request_can_disable_zero_data_retention() {
        let body = build_request("m", "i", &[predicate()], false);
        assert_eq!(
            body["providerOptions"]["gateway"]["zeroDataRetention"],
            false
        );
    }

    #[test]
    fn request_caps_questions_at_sixteen() {
        let many: Vec<Question> = (0..20).map(|_| predicate()).collect();
        let body = build_request("m", "i", &many, true);
        assert_eq!(body["questions"].as_array().unwrap().len(), MAX_QUESTIONS);
    }

    #[test]
    fn response_parses_answers_cost_and_tokens() {
        let body = json!({
            "model": "typesafe-ai/jev",
            "answers": [
                {"type": "predicate", "name": "done", "probability": 0.98},
                {"type": "choice", "name": "kind", "choice": "noise", "confidence": 0.81},
                {"type": "score", "name": "criterion_1", "score": 1.3}
            ],
            "usage": {"input_tokens": 282, "output_tokens": 20, "total_tokens": 302},
            "provider_metadata": {"gateway": {"cost": "0.000011844", "generationId": "gen_1"}}
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.answers.len(), 3);
        assert_eq!(response.cost_usd, 0.000011844);
        assert_eq!(response.input_tokens, 282);
        assert!(matches!(
            response.answers[2],
            Answer::Score { score, confidence: None, .. } if score == 1.3
        ));
    }

    #[test]
    fn response_maps_refusal_and_unknown_answers() {
        let body = json!({"answers": [
            {"type": "refusal", "name": "done"},
            {"type": "something_new"},
            {"type": "predicate"}
        ]});
        let response = parse_response(&body).unwrap();
        assert_eq!(
            response.answers[0],
            Answer::Refusal {
                name: Some("done".into())
            }
        );
        assert_eq!(response.answers[1], Answer::Unknown);
        // A predicate without a probability is malformed, not fatal.
        assert_eq!(response.answers[2], Answer::Unknown);
    }

    #[test]
    fn response_without_answers_is_invalid() {
        assert_eq!(
            parse_response(&json!({"nope": true})),
            Err(Fallback::InvalidAnswer)
        );
        assert_eq!(
            parse_response(&json!({"answers": "x"})),
            Err(Fallback::InvalidAnswer)
        );
    }

    #[test]
    fn response_cost_must_be_a_decimal_string() {
        let number_cost = json!({
            "answers": [],
            "provider_metadata": {"gateway": {"cost": 0.5}}
        });
        assert_eq!(parse_response(&number_cost).unwrap().cost_usd, 0.0);
        let bad = json!({
            "answers": [],
            "provider_metadata": {"gateway": {"cost": "free"}}
        });
        assert_eq!(parse_response(&bad).unwrap().cost_usd, 0.0);
        let negative = json!({
            "answers": [],
            "provider_metadata": {"gateway": {"cost": "-1"}}
        });
        assert_eq!(parse_response(&negative).unwrap().cost_usd, 0.0);
        let missing = json!({"answers": []});
        assert_eq!(parse_response(&missing).unwrap().cost_usd, 0.0);
    }

    #[test]
    fn error_statuses_map_to_http_fallbacks() {
        for code in [401, 402, 403, 429, 500, 503] {
            assert_eq!(parse_error(code), Fallback::Http(code));
        }
    }

    /// Serve one canned HTTP response. Returns the endpoint URL and a channel
    /// that yields the raw request the server received.
    fn serve_once(
        status_line: &'static str,
        body: String,
        delay: Duration,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/decisions", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&received);
                if let Some(split) = text.find("\r\n\r\n") {
                    let content_length = text[..split]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if received.len() >= split + 4 + content_length {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&received).into_owned());
            thread::sleep(delay);
            let reply = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        });
        (endpoint, rx, handle)
    }

    #[test]
    fn send_round_trip_against_a_fake_server() {
        let reply = json!({
            "answers": [{"type": "predicate", "name": "done", "probability": 0.9}],
            "usage": {"input_tokens": 7},
            "provider_metadata": {"gateway": {"cost": "0.0001"}}
        })
        .to_string();
        let (endpoint, rx, handle) = serve_once("HTTP/1.1 200 OK", reply, Duration::ZERO);
        let body = build_request("m", "hello", &[predicate()], true);
        let response = send(&endpoint, "secret-key-123", &body, Duration::from_secs(5)).unwrap();
        handle.join().unwrap();

        assert_eq!(response.cost_usd, 0.0001);
        assert_eq!(response.input_tokens, 7);
        let request = rx.recv().unwrap();
        assert!(request.starts_with("POST /v1/decisions HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer secret-key-123")
        );
        assert!(request.contains("\"zeroDataRetention\":true"));
        assert!(
            !request
                .split("\r\n\r\n")
                .nth(1)
                .unwrap()
                .contains("secret-key-123")
        );
    }

    #[test]
    fn send_maps_http_error_statuses() {
        for (status_line, code) in [
            ("HTTP/1.1 401 Unauthorized", 401),
            ("HTTP/1.1 402 Payment Required", 402),
            ("HTTP/1.1 429 Too Many Requests", 429),
            ("HTTP/1.1 503 Service Unavailable", 503),
        ] {
            let (endpoint, _rx, handle) = serve_once(
                status_line,
                r#"{"error":{"message":"nope","type":"x","code":"y"}}"#.into(),
                Duration::ZERO,
            );
            let body = build_request("m", "i", &[predicate()], true);
            let result = send(&endpoint, "k", &body, Duration::from_secs(5));
            handle.join().unwrap();
            assert_eq!(result, Err(Fallback::Http(code)));
        }
    }

    #[test]
    fn send_rejects_a_non_json_body() {
        let (endpoint, _rx, handle) = serve_once(
            "HTTP/1.1 200 OK",
            "<html>oops</html>".into(),
            Duration::ZERO,
        );
        let body = build_request("m", "i", &[predicate()], true);
        let result = send(&endpoint, "k", &body, Duration::from_secs(5));
        handle.join().unwrap();
        assert_eq!(result, Err(Fallback::InvalidAnswer));
    }

    #[test]
    fn send_times_out_when_the_server_is_slow() {
        let (endpoint, _rx, handle) = serve_once(
            "HTTP/1.1 200 OK",
            r#"{"answers":[]}"#.into(),
            Duration::from_millis(1500),
        );
        let body = build_request("m", "i", &[predicate()], true);
        let started = Instant::now();
        let result = send(&endpoint, "k", &body, Duration::from_millis(250));
        let elapsed = started.elapsed();
        handle.join().unwrap();
        assert_eq!(result, Err(Fallback::Timeout));
        assert!(elapsed < Duration::from_millis(1400), "took {elapsed:?}");
    }

    #[test]
    fn send_reports_connection_refused_without_leaking_the_url() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/decisions", listener.local_addr().unwrap());
        drop(listener);
        let body = build_request("m", "i", &[predicate()], true);
        let result = send(&endpoint, "k", &body, Duration::from_secs(2));
        assert_eq!(result, Err(Fallback::Http(0)));
    }
}
