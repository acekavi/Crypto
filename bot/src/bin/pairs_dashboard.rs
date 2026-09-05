use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_report::write_dashboard;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let profile = Profile::from_name(&std::env::args().nth(1).unwrap_or_else(|| "testnet".into()))?;
    load_env_file("/home/acekavi/Projects/Crypto/.env")?;
    let path = write_dashboard(profile).await?;
    println!("{}", path);
    Ok(())
}
