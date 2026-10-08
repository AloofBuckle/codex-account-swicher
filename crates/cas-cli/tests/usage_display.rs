use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn usage_event(model: &str, id: &str, input: u64, cached: u64, output: u64) -> Vec<Value> {
    vec![
        json!({"type": "turn_context", "payload": {"model": model}}),
        json!({"type": "response_item", "payload": {
            "type": "custom_tool_call", "call_id": id, "name": "functions.exec", "input": "{}"
        }}),
        json!({"type": "token_usage_record", "timestamp": "2026-10-08T11:00:00Z",
            "payload": {"thread_id": "test-thread", "response_id": id,
                "usage": {"input_tokens": input, "cached_input_tokens": cached,
                    "cache_write_input_tokens": 0, "output_tokens": output}
            }
        }),
    ]
}

fn run_usage(name: &str, flags: &[&str], jsonl: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cas"))
        .env("NO_COLOR", "1")
        .env("LANG", "zh_CN.UTF-8")
        .env("LC_ALL", "zh_CN.UTF-8")
        .arg(name)
        .args(flags)
        .arg(jsonl)
        .output()
        .expect("run CAS usage command")
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("UTF-8 usage output")
}

#[test]
fn summary_hides_details_but_detailed_and_json_preserve_everything() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tokens.jsonl");
    let mut lines = vec![json!({"type": "session_meta", "payload": {"id": "test-thread"}})];
    lines.extend(usage_event("gpt-6-sol", "call-1", 236_437, 197_888, 909));
    lines.extend(usage_event(
        "gpt-6.1-sol",
        "call-2",
        1_250_000,
        1_050_000,
        4_500,
    ));
    let mut contents = lines
        .into_iter()
        .map(|entry| serde_json::to_string(&entry).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    // Malformed input must remain available as a warning in Detailed and
    // JSON even though it is intentionally hidden from the Summary UI.
    contents.push_str("\n{\"type\":\"token_usage_record\",not valid json!\n");
    fs::write(&file, contents).unwrap();

    for alias in ["usage", "cost", "price"] {
        let summary = stdout(&run_usage(alias, &[], &file));
        assert_eq!(summary, stdout(&run_usage(alias, &["--summary"], &file)));
        let detailed_run = run_usage(alias, &["--detailed"], &file);
        let detailed = stdout(&detailed_run);
        let detailed_stderr = String::from_utf8(detailed_run.stderr).unwrap();

        assert!(!summary.contains("按模型："), "{alias}: {summary}");
        assert!(!summary.contains("分价："), "{alias}: {summary}");
        assert!(!summary.contains("警告"), "{alias}: {summary}");
        assert!(summary.contains("输入=1.49M"), "{alias}: {summary}");
        assert!(summary.contains("缓存读取=1.25M"), "{alias}: {summary}");
        assert!(summary.contains("平均每工具用量："));
        assert!(summary.contains("缓存命中率="));
        assert!(summary.contains("总价："));
        assert!(summary.contains("已计价=2 / 2"));

        assert!(detailed.contains("按模型："), "{alias}: {detailed}");
        assert!(detailed.contains("分价："), "{alias}: {detailed}");
        assert!(detailed.contains("输入=236437"), "{alias}: {detailed}");
        assert!(detailed.contains("输入=1250000"), "{alias}: {detailed}");
        assert!(
            detailed_stderr.contains("警告1:"),
            "{alias}: {detailed_stderr}"
        );
        assert!(detailed_stderr.contains("malformed"));

        let json_output = stdout(&run_usage(alias, &["--json"], &file));
        let parsed: Value = serde_json::from_str(&json_output).unwrap();
        assert_eq!(
            parsed,
            serde_json::from_str::<Value>(&stdout(&run_usage(
                alias,
                &["--json", "--summary"],
                &file
            )))
            .unwrap()
        );
        assert_eq!(
            parsed,
            serde_json::from_str::<Value>(&stdout(&run_usage(
                alias,
                &["--json", "--detailed"],
                &file
            )))
            .unwrap()
        );
        assert_eq!(parsed["warning_count"], 1);
        assert_eq!(parsed["responses"], 2);
        assert_eq!(parsed["models"][0]["tokens"]["input_tokens"], 236_437);

        assert!(
            !run_usage(alias, &["--summary", "--detailed"], &file)
                .status
                .success()
        );
    }
}
