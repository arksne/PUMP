//! Точка входа сервера PUMP. HTTP и Telegram обслуживает крейт telegram-bridge.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telegram_bridge::run().await
}
