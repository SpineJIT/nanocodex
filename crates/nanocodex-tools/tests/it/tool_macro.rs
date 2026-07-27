use nanocodex_tools::{Tool, ToolContext, ToolInput, tool};
use serde_json::{Value, json, value::to_raw_value};

#[tool(
    name = "deployment_region",
    description = "Returns the configured deployment region."
)]
async fn deployment_region() -> Result<&'static str, &'static str> {
    Ok("us-west-2")
}

#[tokio::test]
async fn macro_works_without_the_nanocodex_facade() {
    let definition = deployment_region.definition();
    assert_eq!(definition.name(), "deployment_region");

    let output = deployment_region
        .execute(
            ToolInput::Function(to_raw_value(&json!({})).unwrap()),
            ToolContext::new(
                "test-model",
                "test-session",
                "test-call",
                &[],
                nanocodex_tools::contract::DEFAULT_TOOL_OUTPUT_TOKENS,
            ),
        )
        .await
        .unwrap();

    assert_eq!(
        serde_json::to_value(output.output).unwrap(),
        Value::String(r#""us-west-2""#.to_owned())
    );
}
