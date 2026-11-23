//! Discord IPC для нативного клиента на Unix.
use crate::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
pub struct DiscordPresence {
    socket: tokio::net::UnixStream,
}
#[cfg(unix)]
impl DiscordPresence {
    pub async fn connect(client_id: &str) -> Result<Self> {
        let root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let mut socket = None;
        for i in 0..10 {
            if let Ok(s) =
                tokio::net::UnixStream::connect(root.join(format!("discord-ipc-{i}"))).await
            {
                socket = Some(s);
                break;
            }
        }
        let mut this = Self {
            socket: socket.ok_or_else(|| Error::Unsupported("Discord IPC недоступен".into()))?,
        };
        this.send(0, &serde_json::json!({"v":1,"client_id":client_id}))
            .await?;
        this.receive().await?;
        Ok(this)
    }
    async fn send(&mut self, opcode: u32, value: &serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        self.socket.write_u32_le(opcode).await?;
        self.socket.write_u32_le(bytes.len() as u32).await?;
        self.socket.write_all(&bytes).await?;
        Ok(())
    }
    async fn receive(&mut self) -> Result<serde_json::Value> {
        let _opcode = self.socket.read_u32_le().await?;
        let size = self.socket.read_u32_le().await?;
        if size > 1024 * 1024 {
            return Err(Error::Protocol("IPC frame too large".into()));
        }
        let mut bytes = vec![0; size as usize];
        self.socket.read_exact(&mut bytes).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub async fn listening(&mut self, title: &str, artist: &str) -> Result<()> {
        self.send(1,&serde_json::json!({"cmd":"SET_ACTIVITY","args":{"pid":std::process::id(),"activity":{"type":2,"details":title,"state":artist}},"nonce":format!("{}",std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos())})).await?;
        let reply = self.receive().await?;
        if reply["evt"] == "ERROR" {
            return Err(Error::Protocol("Discord rejected activity".into()));
        }
        Ok(())
    }
}
