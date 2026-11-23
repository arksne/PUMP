//! Пульт Telegram использует авторизованный WebSocket пользователя Mini App.
use crate::{ApiError, App};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, broadcast};

#[derive(Clone, Default)]
pub(crate) struct Controls(Arc<Mutex<HashMap<String, broadcast::Sender<Command>>>>);

#[derive(Clone, Serialize)]
pub(crate) struct Command {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bands: Option<Vec<f32>>,
}
impl Command {
    pub fn new(action: &'static str) -> Self {
        Self {
            kind: "command",
            action,
            track_id: None,
            position_ms: None,
            bands: None,
        }
    }
}
impl Controls {
    pub async fn send(&self, owner: &str, command: Command) -> bool {
        self.0
            .lock()
            .await
            .get(owner)
            .is_some_and(|sender| sender.send(command).is_ok())
    }
    async fn subscribe(&self, owner: String) -> Result<broadcast::Receiver<Command>, ApiError> {
        let mut channels = self.0.lock().await;
        channels.retain(|_, channel| channel.receiver_count() > 0);
        if channels.len() >= 1024 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Слишком много подключений пульта.",
            ));
        }
        Ok(channels
            .entry(owner)
            .or_insert_with(|| broadcast::channel(32).0)
            .subscribe())
    }
}
#[derive(Deserialize)]
struct Join {
    #[serde(rename = "type")]
    kind: String,
    token: String,
}

pub(crate) async fn upgrade(State(app): State<App>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(4096)
        .on_upgrade(move |socket| serve(socket, app))
}
async fn serve(mut socket: WebSocket, app: App) {
    let Ok(Some(Ok(Message::Text(input)))) =
        tokio::time::timeout(Duration::from_secs(5), socket.recv()).await
    else {
        return;
    };
    let Ok(join) = serde_json::from_str::<Join>(&input) else {
        return;
    };
    if join.kind != "join" {
        return;
    }
    let Ok(Some(owner)) = app.stats.owner(&join.token).await else {
        return;
    };
    let Ok(mut commands) = app.controls.subscribe(owner).await else {
        return;
    };
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    let mut active = tokio::time::Instant::now();
    loop {
        tokio::select! {
            input=socket.recv()=>{
                match input { Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break, _=>active=tokio::time::Instant::now() }
            },
            command=commands.recv()=>{
                let Ok(command)=command else {break;};
                let Ok(json)=serde_json::to_string(&command) else {continue;};
                if !matches!(tokio::time::timeout(Duration::from_secs(4),socket.send(Message::Text(json.into()))).await,Ok(Ok(()))) {break;}
            },
            _=heartbeat.tick()=>{
                if active.elapsed()>Duration::from_secs(60) {break;}
                if !matches!(tokio::time::timeout(Duration::from_secs(4),socket.send(Message::Ping(Default::default()))).await,Ok(Ok(()))) {break;}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn commands_only_reach_the_owners_channel() {
        let controls = Controls::default();
        let mut receiver = controls.subscribe("alice".into()).await.unwrap();
        assert!(!controls.send("bob", Command::new("pause")).await);
        assert!(controls.send("alice", Command::new("pause")).await);
        assert_eq!(receiver.recv().await.unwrap().action, "pause");
    }
}
