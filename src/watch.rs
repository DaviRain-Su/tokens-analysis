//! 钱包监控：轮询目标钱包的新交易，解析买卖动向，可选触发跟单。

use crate::pnl::parse_wallet_events;
use crate::rpc::Rpc;
use crate::trade::Executor;
use crate::types::{Side, WatchEvent, fmt_time, human, short};
use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

pub struct Watcher {
    /// 钱包 → 已见过的最新签名（基线，启动时不回放历史）
    cursor: HashMap<String, Option<String>>,
}

impl Watcher {
    pub async fn new(rpc: &Rpc, wallets: &[String]) -> Result<Self> {
        let mut cursor = HashMap::new();
        for w in wallets {
            let sigs = rpc
                .call("getSignaturesForAddress", json!([w, {"limit": 1}]))
                .await?;
            let latest = sigs[0]["signature"].as_str().map(String::from);
            cursor.insert(w.clone(), latest);
        }
        Ok(Self { cursor })
    }

    pub async fn run(
        &mut self,
        rpc: &Rpc,
        interval: u64,
        mut executor: Option<&mut Executor>,
    ) -> Result<()> {
        println!(
            "开始监控 {} 个钱包 (每 {interval}s 轮询, Ctrl-C 退出)...",
            self.cursor.len()
        );
        loop {
            let wallets: Vec<String> = self.cursor.keys().cloned().collect();
            for w in wallets {
                match self.poll_wallet(rpc, &w).await {
                    Ok(events) => {
                        for ev in &events {
                            print_event(&w, ev);
                            if let Some(exec) = executor.as_deref_mut() {
                                exec.on_event(rpc, &w, ev).await;
                            }
                        }
                    }
                    Err(e) => eprintln!("⚠ 轮询 {} 失败: {e}", short(&w)),
                }
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    }

    /// 拉取一个钱包自上次以来的新交易，按时间顺序返回事件。
    async fn poll_wallet(&mut self, rpc: &Rpc, wallet: &str) -> Result<Vec<WatchEvent>> {
        let last = self.cursor.get(wallet).cloned().flatten();
        let sigs = rpc
            .call("getSignaturesForAddress", json!([wallet, {"limit": 25}]))
            .await?;
        let arr = sigs.as_array().cloned().unwrap_or_default();
        // 收集基线之后的新签名（最新在前 → 反转成时间顺序）
        let mut fresh: Vec<String> = Vec::new();
        for s in &arr {
            let sig = s["signature"].as_str().unwrap_or_default();
            if Some(sig) == last.as_deref() {
                break;
            }
            if s["err"].is_null() {
                fresh.push(sig.to_string());
            }
        }
        if let Some(newest) = arr.first().and_then(|s| s["signature"].as_str()) {
            self.cursor.insert(wallet.to_string(), Some(newest.into()));
        }
        fresh.reverse();

        let mut events = Vec::new();
        for sig in &fresh {
            let tx = rpc.transaction(sig).await?;
            events.extend(parse_wallet_events(&tx, wallet));
        }
        Ok(events)
    }
}

fn print_event(wallet: &str, ev: &WatchEvent) {
    let e = &ev.event;
    let (tag, color) = match e.side {
        Side::Buy => ("买入", "\x1b[32m"),
        Side::Sell => ("卖出", "\x1b[31m"),
        Side::TransferIn => ("转入", "\x1b[36m"),
        Side::TransferOut => ("转出", "\x1b[33m"),
    };
    let value = if e.sol_amount > 0.0 {
        format!(" {:.4} SOL", e.sol_amount)
    } else if e.usd_amount > 0.0 {
        format!(" ${:.2}", e.usd_amount)
    } else {
        String::new()
    };
    let price = e
        .price_sol
        .map(|p| format!(" @{p:.10}"))
        .unwrap_or_default();
    println!(
        "{} {} {color}{tag}\x1b[0m {} {}{value}{price}  (余额 {} → {})  {}",
        fmt_time(e.time),
        short(wallet),
        human(e.token_amount),
        short(&ev.mint),
        human(ev.pre_balance),
        human(ev.post_balance),
        short(&e.signature),
    );
}
