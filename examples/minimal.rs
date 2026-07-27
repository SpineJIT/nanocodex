use nanocodex::Nanocodex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("OPENAI_API_KEY")?;
    let (agent, _) = Nanocodex::new(api_key)?;

    let turn = agent
        .prompt("Inspect this repository and summarize it.")
        .await?;
    let result = turn.result().await?;
    println!("{}", result.final_message());
    Ok(())
}
