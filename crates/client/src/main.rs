use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cloudcode", about = "Cloudcode client: launch AI CLI tools via cloudcode hub")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动一个 AI CLI 工具，自动注入 hub 配置
    Run {
        /// 工具名称（MVP 仅支持 claude）
        tool: String,
        /// 透传给工具的参数
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// 显示当前 client 配置
    Config,
}

#[derive(serde::Deserialize, Debug)]
struct ClientConfig {
    hub_url: String,
    token: String,
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not find user config dir")?;
    Ok(dir.join("cloudcode").join("config.toml"))
}

fn load_config() -> Result<ClientConfig> {
    let path = config_path()?;
    let s = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} (run `cloudcode config` for instructions)",
            path.display()
        )
    })?;
    Ok(toml::from_str(&s)?)
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Run { tool, args } => run(&tool, args),
        Cmd::Config => show_config(),
    }
}

fn show_config() -> Result<()> {
    let path = config_path()?;
    println!("config file: {}", path.display());
    match load_config() {
        Ok(c) => {
            println!("hub_url: {}", c.hub_url);
            let masked: String = c.token.chars().take(10).collect();
            println!("token:   {}...", masked);
        }
        Err(_) => {
            println!();
            println!("config not found. create with:");
            println!("  mkdir -p {}", path.parent().unwrap().display());
            println!("  cat > {} <<EOF", path.display());
            println!("  hub_url = \"http://localhost:7000\"");
            println!("  token = \"cc_xxx\"");
            println!("  EOF");
        }
    }
    Ok(())
}

fn run(tool: &str, args: Vec<String>) -> Result<()> {
    let cfg = load_config()?;

    let (program, env): (&str, Vec<(&str, String)>) = match tool {
        "claude" => (
            "claude",
            vec![
                (
                    "ANTHROPIC_BASE_URL",
                    format!("{}/anthropic", cfg.hub_url.trim_end_matches('/')),
                ),
                ("ANTHROPIC_AUTH_TOKEN", cfg.token.clone()),
            ],
        ),
        other => anyhow::bail!("unsupported tool '{}'. MVP supports: claude", other),
    };

    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(&args);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let err = cmd.exec();
    Err(anyhow::anyhow!("exec {}: {}", program, err))
}
