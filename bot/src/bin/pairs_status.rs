use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_report::{collect_status, render_text};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let profile = Profile::from_name(&args.next().unwrap_or_else(|| "testnet".into()))?;
    load_env_file("/home/acekavi/Projects/Crypto/.env")?;
    let json = args.any(|a| a == "--json");
    let status = collect_status(profile, false).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("{}", render_text(&status));
    }
    Ok(())
}
