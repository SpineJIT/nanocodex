use criterion::{Criterion, criterion_group, criterion_main};
use nanocodex_tools::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolContext, ToolInput, ToolRuntime};
use serde_json::{json, value::to_raw_value};

fn benchmark_process_output(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark Tokio runtime must initialize");
    let tools = ToolRuntime::new(".", None, None);
    let input = to_raw_value(&json!({
        "cmd": "printf '%065536d' 0",
        "login": false,
        "max_output_tokens": 1_024
    }))
    .expect("benchmark input must serialize");
    let context = ToolContext::new(
        "benchmark-model",
        "benchmark-session",
        "benchmark-process",
        &[],
        DEFAULT_TOOL_OUTPUT_TOKENS,
    );

    c.bench_function("tool_process_output/64k_to_1k_tokens", |benchmark| {
        benchmark.to_async(&runtime).iter(|| async {
            let output = tools
                .execute_tool("exec_command", ToolInput::Function(input.clone()), context)
                .await;
            assert!(output.success);
            std::hint::black_box(output);
        });
    });
}

criterion_group!(benches, benchmark_process_output);
criterion_main!(benches);
