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
    /// mint → symbol 缓存（None 表示已查过但没有元数据）
    symbols: HashMap<String, Option<String>>,
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
        Ok(Self {
            cursor,
            symbols: HashMap::new(),
        })
    }

    /// 解析 mint 的 symbol（带缓存），填充到事件上。
    async fn fill_symbols(&mut self, rpc: &Rpc, events: &mut [WatchEvent]) {
        for ev in events {
            if !self.symbols.contains_key(&ev.mint) {
                let sym = crate::meta::fetch_meta(rpc, &ev.mint)
                    .await
                    .map(|m| m.symbol)
                    .filter(|s| !s.is_empty());
                self.symbols.insert(ev.mint.clone(), sym);
            }
            ev.symbol = self.symbols.get(&ev.mint).cloned().flatten();
        }
    }

    /// 轮询模式（WebSocket 不可用时的回退路径）
    pub async fn run_polling(
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
                    Ok(mut events) => {
                        self.fill_symbols(rpc, &mut events).await;
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
            if let Some(exec) = executor.as_deref_mut() {
                exec.check_positions(rpc).await;
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    }

    /// WebSocket 实时模式：logsSubscribe 推送签名，亚秒级延迟。
    pub async fn run_ws(
        &mut self,
        rpc: &Rpc,
        ws_url: String,
        price_check_interval: u64,
        mut executor: Option<&mut Executor>,
    ) -> Result<()> {
        let wallets: Vec<String> = self.cursor.keys().cloned().collect();
        println!(
            "开始监控 {} 个钱包 (WebSocket 实时推送, Ctrl-C 退出)...",
            wallets.len()
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, String)>(256);
        tokio::spawn(crate::ws::subscribe_task(ws_url, wallets, tx));

        // 同一笔交易可能涉及多个监控钱包，按签名去重（保留最近 512 个）
        let mut seen: std::collections::VecDeque<String> = Default::default();
        let mut tick =
            tokio::time::interval(Duration::from_secs(price_check_interval.max(5)));
        tick.tick().await;

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Some(exec) = executor.as_deref_mut() {
                        exec.check_positions(rpc).await;
                    }
                }
                msg = rx.recv() => {
                    let Some((wallet, sig)) = msg else {
                        anyhow::bail!("WebSocket 任务退出");
                    };
                    if seen.contains(&sig) {
                        continue;
                    }
                    seen.push_back(sig.clone());
                    if seen.len() > 512 {
                        seen.pop_front();
                    }
                    match fetch_tx_with_retry(rpc, &sig).await {
                        Ok(tx_data) => {
                            let mut events = parse_wallet_events(&tx_data, &wallet);
                            self.fill_symbols(rpc, &mut events).await;
                            for ev in &events {
                                print_event(&wallet, ev);
                                if let Some(exec) = executor.as_deref_mut() {
                                    exec.on_event(rpc, &wallet, ev).await;
                                }
                            }
                        }
                        Err(e) => eprintln!("⚠ 获取交易 {} 失败: {e}", short(&sig)),
                    }
                }
            }
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

/// 通知推送先于交易可查（索引滞后），带退避重试。
async fn fetch_tx_with_retry(rpc: &Rpc, sig: &str) -> Result<serde_json::Value> {
    let mut delay = Duration::from_millis(300);
    for _ in 0..5 {
        let tx = rpc.transaction(sig).await?;
        if !tx.is_null() {
            return Ok(tx);
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(3));
    }
    anyhow::bail!("交易在重试后仍不可查")
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
    let token_disp = match &ev.symbol {
        Some(s) => format!("{s}({})", short(&ev.mint)),
        None => short(&ev.mint),
    };
    println!(
        "{} {} {color}{tag}\x1b[0m {} {token_disp}{value}{price}  (余额 {} → {})  {}",
        fmt_time(e.time),
        short(wallet),
        human(e.token_amount),
        human(ev.pre_balance),
        human(ev.post_balance),
        short(&e.signature),
    );
}
