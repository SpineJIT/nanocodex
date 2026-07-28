use nanocodex::{
    Tool, tool,
    tools::{ToolContext, ToolInput, contract::DEFAULT_TOOL_OUTPUT_TOKENS},
};
use serde_json::{Value, json, value::to_raw_value};

#[tool(name = "add_numbers", description = "Adds two signed integers.")]
async fn add(left: i64, right: i64) -> Result<i64, &'static str> {
    left.checked_add(right).ok_or("integer addition overflowed")
}

#[tokio::test]
async fn macro_generates_schema_and_executes_through_public_tool_trait() {
    let definition = serde_json::to_value(add.definition()).unwrap();
    assert_eq!(definition["name"], "add_numbers");
    assert_eq!(definition["parameters"]["type"], "object");
    assert_eq!(
        definition["parameters"]["required"],
        json!(["left", "right"])
    );
    assert_eq!(definition["output_schema"]["type"], "integer");

    let execution = add
        .execute(
            ToolInput::Function(to_raw_value(&json!({ "left": 20, "right": 22 })).unwrap()),
            ToolContext::new(
                "test-model",
                "test-session",
                "test-call",
                &[],
                DEFAULT_TOOL_OUTPUT_TOKENS,
            ),
        )
        .await
        .unwrap();
    assert!(execution.success);
    assert_eq!(
        serde_json::to_value(execution.output).unwrap(),
        Value::String("42".into())
    );

    let overflow = add
        .execute(
            ToolInput::Function(to_raw_value(&json!({ "left": i64::MAX, "right": 1 })).unwrap()),
            ToolContext::new(
                "test-model",
                "test-session",
                "overflow-call",
                &[],
                DEFAULT_TOOL_OUTPUT_TOKENS,
            ),
        )
        .await;
    let Err(error) = overflow else {
        panic!("overflowing tool call unexpectedly succeeded");
    };
    assert_eq!(error.to_string(), "integer addition overflowed");
}
