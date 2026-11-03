use anyhow::{Context, Result};
#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut set = false;
    let mut stdin = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                path = Some(std::path::PathBuf::from(
                    args.next().context("--config requires a path")?,
                ))
            }
            "--set-password" => set = true,
            "--password-stdin" => stdin = true,
            "--help" | "-h" => {
                println!("astra-web --config web.toml\nastra-web --config web.toml --set-password --password-stdin\nIndependent authenticated web gateway; the manager must already be running.");
                return Ok(());
            }
            _ => anyhow::bail!("unknown option: {arg}"),
        }
    }
    let config = astra_web::config::Config::load(&path.context("--config is required")?)?;
    if set {
        anyhow::ensure!(
            stdin,
            "use --password-stdin; passwords must not be command-line arguments"
        );
        use std::io::Read;
        let mut password = String::new();
        std::io::stdin().take(1026).read_to_string(&mut password)?;
        astra_web::auth::set_password(
            &config.password_file(),
            password.trim_end_matches(['\r', '\n']),
        )?;
        println!("Administrator credential saved; previous sessions are revoked.");
        return Ok(());
    }
    astra_web::run(config).await
}
