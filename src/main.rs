//! 在宿主提供的不可变配置和受管 RPC 会话中运行额度保护插件

use cpr_quota_guard::{build, config::Config};
use gateway_plugin_sdk::client::{PluginSession, SessionConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    let config = Config::from_value(session.handshake().configuration.clone())
        .map_err(std::io::Error::other)?;
    let plugin = build(config, session.handshake().artifact_sha256.clone())?;
    session.run(plugin).await?;
    Ok(())
}
