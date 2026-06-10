//! 跟单执行器：Jupiter Swap API 报价/组交易 → 本地签名 → 发送确认。
//!
//! 安全护栏：
//! - 默认 paper 模式，只记录决策不发交易；--live 才真实下单
//! - 单笔固定 SOL 金额 + 每日总额上限 + 滑点上限
//! - 低于 --min-trigger-sol 的信号忽略（灰尘/试探单不跟）
//! - 跟卖只卖出本工具买入的仓位，比例跟随目标钱包
//! - 每个决策（含跳过原因）追加写入 JSONL 审计日志

use crate::rpc::Rpc;
use crate::types::{Side, WatchEvent, short};
use crate::wallet::Wallet;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;

pub struct ExecConfig {
    pub live: bool,
    pub buy_sol: f64,
    pub max_daily_sol: f64,
    pub slippage_bps: u32,
    pub min_trigger_sol: f64,
    pub sell_full: bool,
    pub jupiter: String,
    pub log_path: String,
    /// SOL/USD 汇率，用于把稳定币计价的买入信号换算成 SOL 后判断触发阈值
    pub sol_usd: Option<f64>,
}

pub struct Executor {
    cfg: ExecConfig,
    http: reqwest::Client,
    wallet: Wallet,
    /// 本工具买入的仓位: mint → 原始单位数量 (raw amount)
    positions: HashMap<String, u64>,
    spent_today: f64,
    day: String,
}

const WSOL: &str = "So11111111111111111111111111111111111111112";

impl Executor {
    pub fn new(cfg: ExecConfig, wallet: Wallet) -> Self {
        Self {
            cfg,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("http client"),
            wallet,
            positions: HashMap::new(),
            spent_today: 0.0,
            day: String::new(),
        }
    }

    pub fn pubkey(&self) -> &str {
        &self.wallet.pubkey
    }

    pub async fn on_event(&mut self, rpc: &Rpc, src_wallet: &str, ev: &WatchEvent) {
        let res = match ev.event.side {
            Side::Buy => self.maybe_copy_buy(rpc, src_wallet, ev).await,
            Side::Sell => self.maybe_copy_sell(rpc, src_wallet, ev).await,
            _ => return,
        };
        if let Err(e) = res {
            eprintln!("  ✗ 跟单失败: {e}");
            self.audit(json!({"action": "error", "mint": ev.mint, "error": e.to_string()}));
        }
    }

    async fn maybe_copy_buy(&mut self, rpc: &Rpc, src: &str, ev: &WatchEvent) -> Result<()> {
        // 信号规模换算成 SOL（稳定币买入按汇率折算）
        let signal_sol = ev.event.sol_amount
            + self
                .cfg
                .sol_usd
                .map(|r| ev.event.usd_amount / r)
                .unwrap_or(0.0);
        if signal_sol < self.cfg.min_trigger_sol {
            return Ok(()); // 信号太小，不跟
        }
        self.roll_day();
        if self.spent_today + self.cfg.buy_sol > self.cfg.max_daily_sol {
            println!(
                "  ⚠ 跳过跟买 {}: 今日已用 {:.3}/{:.3} SOL",
                short(&ev.mint),
                self.spent_today,
                self.cfg.max_daily_sol
            );
            self.audit(json!({"action": "skip_buy", "mint": ev.mint, "reason": "daily_cap"}));
            return Ok(());
        }
        let lamports = (self.cfg.buy_sol * 1e9) as u64;
        let quote = self.quote(WSOL, &ev.mint, lamports).await?;
        let out_amount: u64 = quote["outAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("报价缺少 outAmount"))?;
        let impact = quote["priceImpactPct"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        println!(
            "  → 跟买 {}: {:.4} SOL (冲击 {:.2}%) {}",
            short(&ev.mint),
            self.cfg.buy_sol,
            impact * 100.0,
            if self.cfg.live { "[LIVE]" } else { "[paper]" }
        );
        self.audit(json!({
            "action": "copy_buy", "src": src, "mint": ev.mint,
            "sol": self.cfg.buy_sol, "quote_out": out_amount,
            "mode": if self.cfg.live { "live" } else { "paper" },
        }));
        if !self.cfg.live {
            self.spent_today += self.cfg.buy_sol;
            *self.positions.entry(ev.mint.clone()).or_default() += out_amount;
            return Ok(());
        }
        let sig = self.swap_and_send(rpc, &quote).await?;
        self.spent_today += self.cfg.buy_sol;
        *self.positions.entry(ev.mint.clone()).or_default() += out_amount;
        println!("  ✓ 买入已确认: {sig}");
        self.audit(json!({"action": "buy_confirmed", "mint": ev.mint, "sig": sig}));
        Ok(())
    }

    async fn maybe_copy_sell(&mut self, rpc: &Rpc, src: &str, ev: &WatchEvent) -> Result<()> {
        let held = *self.positions.get(&ev.mint).unwrap_or(&0);
        if held == 0 {
            return Ok(()); // 不持有此代币（只卖本工具买入的仓位）
        }
        // 比例跟随：目标卖了持仓的多少比例，我们也卖多少
        let their_pre = ev.pre_balance.max(f64::MIN_POSITIVE);
        let fraction = if self.cfg.sell_full {
            1.0
        } else {
            (ev.event.token_amount / their_pre).clamp(0.0, 1.0)
        };
        let sell_raw = ((held as f64) * fraction) as u64;
        if sell_raw == 0 {
            return Ok(());
        }
        let quote = self.quote(&ev.mint, WSOL, sell_raw).await?;
        let out_sol = quote["outAmount"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0) as f64
            / 1e9;
        println!(
            "  → 跟卖 {}: {:.0}% 仓位 ≈ {:.4} SOL {}",
            short(&ev.mint),
            fraction * 100.0,
            out_sol,
            if self.cfg.live { "[LIVE]" } else { "[paper]" }
        );
        self.audit(json!({
            "action": "copy_sell", "src": src, "mint": ev.mint,
            "fraction": fraction, "sell_raw": sell_raw, "est_sol": out_sol,
            "mode": if self.cfg.live { "live" } else { "paper" },
        }));
        if !self.cfg.live {
            *self.positions.get_mut(&ev.mint).unwrap() = held - sell_raw;
            return Ok(());
        }
        let sig = self.swap_and_send(rpc, &quote).await?;
        *self.positions.get_mut(&ev.mint).unwrap() = held - sell_raw;
        println!("  ✓ 卖出已确认: {sig}");
        self.audit(json!({"action": "sell_confirmed", "mint": ev.mint, "sig": sig}));
        Ok(())
    }

    async fn quote(&self, input: &str, output: &str, amount: u64) -> Result<Value> {
        let url = format!(
            "{}/quote?inputMint={input}&outputMint={output}&amount={amount}&slippageBps={}",
            self.cfg.jupiter, self.cfg.slippage_bps
        );
        let v: Value = self.http.get(&url).send().await?.json().await?;
        if v.get("error").is_some() {
            bail!("Jupiter 报价失败: {}", v["error"]);
        }
        Ok(v)
    }

    /// 拿 swap 交易 → 签名 → 发送 → 等确认，返回签名。
    async fn swap_and_send(&self, rpc: &Rpc, quote: &Value) -> Result<String> {
        let body = json!({
            "quoteResponse": quote,
            "userPublicKey": self.wallet.pubkey,
            "wrapAndUnwrapSol": true,
            "dynamicComputeUnitLimit": true,
            "prioritizationFeeLamports": "auto",
        });
        let v: Value = self
            .http
            .post(format!("{}/swap", self.cfg.jupiter))
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        let tx_b64 = v["swapTransaction"]
            .as_str()
            .ok_or_else(|| anyhow!("Jupiter swap 响应异常: {v}"))?;
        let signed = self.wallet.sign_transaction_b64(tx_b64)?;
        let sig = rpc
            .call(
                "sendTransaction",
                json!([signed, {"encoding": "base64", "skipPreflight": false, "maxRetries": 3}]),
            )
            .await?;
        let sig = sig.as_str().ok_or_else(|| anyhow!("sendTransaction 未返回签名"))?.to_string();
        // 轮询确认，最多 60 秒
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let st = rpc
                .call("getSignatureStatuses", json!([[sig]]))
                .await?;
            let s = &st["value"][0];
            if !s.is_null() {
                if !s["err"].is_null() {
                    bail!("交易上链但执行失败: {} ({sig})", s["err"]);
                }
                let status = s["confirmationStatus"].as_str().unwrap_or("");
                if status == "confirmed" || status == "finalized" {
                    return Ok(sig);
                }
            }
        }
        bail!("交易确认超时: {sig}")
    }

    fn roll_day(&mut self) {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        if self.day != today {
            self.day = today;
            self.spent_today = 0.0;
        }
    }

    fn audit(&self, mut entry: Value) {
        entry["ts"] = json!(chrono::Utc::now().to_rfc3339());
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.cfg.log_path)
        {
            let _ = writeln!(f, "{entry}");
        }
    }
}
