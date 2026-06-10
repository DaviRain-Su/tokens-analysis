//! 通知推送：macOS 桌面通知 / Telegram Bot。
//! 失败只打警告，绝不阻塞交易主流程。

#[derive(Clone)]
pub enum Notifier {
    Desktop,
    Telegram { token: String, chat_id: String },
}

impl Notifier {
    /// `--notify desktop` 或 `--notify telegram`（后者读 env
    /// TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID）
    pub fn from_kind(kind: &str) -> anyhow::Result<Self> {
        match kind {
            "desktop" => Ok(Self::Desktop),
            "telegram" => {
                let token = std::env::var("TELEGRAM_BOT_TOKEN")
                    .map_err(|_| anyhow::anyhow!("缺少环境变量 TELEGRAM_BOT_TOKEN"))?;
                let chat_id = std::env::var("TELEGRAM_CHAT_ID")
                    .map_err(|_| anyhow::anyhow!("缺少环境变量 TELEGRAM_CHAT_ID"))?;
                Ok(Self::Telegram { token, chat_id })
            }
            other => anyhow::bail!("未知通知方式: {other} (支持 desktop | telegram)"),
        }
    }

    /// 异步发出通知（后台执行，不等待结果）。
    pub fn send(&self, title: &str, body: &str) {
        match self {
            Self::Desktop => {
                let script = format!(
                    "display notification \"{}\" with title \"{}\"",
                    body.replace('"', "'"),
                    title.replace('"', "'")
                );
                tokio::task::spawn_blocking(move || {
                    let _ = std::process::Command::new("osascript")
                        .arg("-e")
                        .arg(&script)
                        .output();
                });
            }
            Self::Telegram { token, chat_id } => {
                let url = format!("https://api.telegram.org/bot{token}/sendMessage");
                let body = serde_json::json!({
                    "chat_id": chat_id,
                    "text": format!("{title}\n{body}"),
                });
                tokio::spawn(async move {
                    let client = reqwest::Client::new();
                    if let Err(e) = client.post(&url).json(&body).send().await {
                        eprintln!("⚠ Telegram 通知失败: {e}");
                    }
                });
            }
        }
    }
}
