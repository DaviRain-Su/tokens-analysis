//! WebSocket 实时订阅：对每个监控钱包 logsSubscribe，
//! 新交易的签名亚秒级推送到 channel。断线自动重连并重新订阅。

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// 从 HTTP RPC URL 推导 WebSocket URL（Triton 等主流提供商同路径支持 wss)
pub fn derive_ws_url(http_url: &str) -> String {
    http_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1)
}

/// 后台任务：维持订阅，把 (钱包, 签名) 发到 channel。永不返回（除非 channel 关闭）。
pub async fn subscribe_task(ws_url: String, wallets: Vec<String>, tx: mpsc::Sender<(String, String)>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_connection(&ws_url, &wallets, &tx).await {
            Ok(()) => return, // channel 关闭，正常退出
            Err(e) => {
                eprintln!("⚠ WebSocket 断开: {e}，{}s 后重连", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn run_connection(
    ws_url: &str,
    wallets: &[String],
    tx: &mpsc::Sender<(String, String)>,
) -> Result<()> {
    let (stream, _) = connect_async(ws_url).await?;
    let (mut write, mut read) = stream.split();

    // 逐钱包订阅；请求 id = 钱包下标 + 1
    for (i, w) in wallets.iter().enumerate() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": i + 1,
            "method": "logsSubscribe",
            "params": [{"mentions": [w]}, {"commitment": "confirmed"}]
        });
        write.send(Message::Text(req.to_string())).await?;
    }

    // 订阅号 → 钱包
    let mut sub_map: HashMap<u64, String> = HashMap::new();
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await; // 跳过立刻触发的第一次

    loop {
        tokio::select! {
            _ = ping.tick() => {
                write.send(Message::Ping(vec![])).await?;
            }
            msg = read.next() => {
                let Some(msg) = msg else {
                    anyhow::bail!("连接被服务端关闭");
                };
                match msg? {
                    Message::Text(text) => {
                        let v: Value = serde_json::from_str(&text)?;
                        // 订阅确认: {"id": n, "result": <subscription>}
                        if let (Some(id), Some(sub)) = (v["id"].as_u64(), v["result"].as_u64()) {
                            if let Some(w) = wallets.get((id - 1) as usize) {
                                sub_map.insert(sub, w.clone());
                            }
                            continue;
                        }
                        if v["method"].as_str() != Some("logsNotification") {
                            continue;
                        }
                        let params = &v["params"];
                        let Some(wallet) = params["subscription"]
                            .as_u64()
                            .and_then(|s| sub_map.get(&s))
                        else {
                            continue;
                        };
                        let value = &params["result"]["value"];
                        if !value["err"].is_null() {
                            continue; // 失败交易
                        }
                        let Some(sig) = value["signature"].as_str() else {
                            continue;
                        };
                        if tx.send((wallet.clone(), sig.to_string())).await.is_err() {
                            return Ok(()); // 接收端退出
                        }
                    }
                    Message::Ping(data) => write.send(Message::Pong(data)).await?,
                    Message::Close(_) => anyhow::bail!("收到 Close 帧"),
                    _ => {}
                }
            }
        }
    }
}
