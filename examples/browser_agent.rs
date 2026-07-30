use eyre::{Result, WrapErr};
use nanocodex::{Nanocodex, OpenAi, Thinking, Tools};
use nanocodex_browser::BrowserTool;

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("OPENAI_API_KEY").wrap_err("OPENAI_API_KEY is required")?;
    let browser = BrowserTool::new()?;
    let tools = Tools::builder().tool(browser).build()?;
    let openai = OpenAi::new(api_key)?;
    let (agent, mut events) = Nanocodex::builder(openai)
        .instructions(
            "Use `tools.browser` from Code Mode for browser work. Inspect the page after every navigation before interacting with it.",
        )
        .thinking(Thinking::Low)
        .tools(tools)
        .build()?;

    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.is_empty() {
        "Open https://example.com, inspect the page, and report its main heading."
    } else {
        &prompt
    };
    let turn = agent.prompt(prompt).await?;
    events.write_turn_jsonl(std::io::stdout()).await?;
    let result = turn.result().await?;
    eprintln!("final result: {}", result.final_message());
    Ok(())
}
