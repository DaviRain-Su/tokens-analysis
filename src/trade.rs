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
use serde::{Deserialize, Serialize};
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
    /// 跳过安全检查照常跟买（不建议）
    pub allow_risky: bool,
    /// 止盈倍数 (如 2.0 = 现值达到成本 2 倍时清仓)，0 = 关闭
    pub take_profit: f64,
    /// 止损倍数 (如 0.5 = 现值跌到成本一半时清仓)，0 = 关闭
    pub stop_loss: f64,
    /// 仓位持久化文件
    pub positions_path: String,
    /// 买卖执行后推送通知
    pub notifier: Option<crate::notify::Notifier>,
    /// TUI 仪表盘模式：不往 stdout 打印，动作进缓冲由仪表盘渲染
    pub quiet: bool,
    /// 即使没开止盈止损也巡检持仓现值（仪表盘需要）
    pub value_positions: bool,
}

/// 本工具买入的单个仓位
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Position {
    /// 原始单位数量 (raw amount)
    pub raw: u64,
    /// 累计 SOL 成本
    pub cost_sol: f64,
    /// 最近一次巡检的可变现价值 (Jupiter 报价)
    #[serde(skip)]
    pub last_value_sol: Option<f64>,
}

pub struct Executor {
    cfg: ExecConfig,
    http: reqwest::Client,
    wallet: Wallet,
    positions: HashMap<String, Position>,
    /// 安全检查结果缓存: mint → 是否通过
    safety_cache: HashMap<String, bool>,
    /// 最近的跟单动作（仪表盘展示）
    actions: std::collections::VecDeque<String>,
    spent_today: f64,
    day: String,
}

const WSOL: &str = "So11111111111111111111111111111111111111112";

fn load_positions(path: &str) -> HashMap<String, Position> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_roundtrip() {
        let path = std::env::temp_dir().join("ta-positions-test.json");
        let path = path.to_str().unwrap();
        let mut m = HashMap::new();
        m.insert(
            "MintA".to_string(),
            Position { raw: 12345, cost_sol: 0.05, last_value_sol: None },
        );
        std::fs::write(path, serde_json::to_string(&m).unwrap()).unwrap();
        let loaded = load_positions(path);
        assert_eq!(loaded["MintA"].raw, 12345);
        assert!((loaded["MintA"].cost_sol - 0.05).abs() < 1e-12);
        let _ = std::fs::remove_file(path);
        // 文件不存在 → 空仓位
        assert!(load_positions("/nonexistent/positions.json").is_empty());
    }
}

impl Executor {
    pub fn new(cfg: ExecConfig, wallet: Wallet) -> Self {
        let positions = load_positions(&cfg.positions_path);
        if !positions.is_empty() {
            println!(
                "已从 {} 恢复 {} 个仓位",
                cfg.positions_path,
                positions.len()
            );
        }
        Self {
            positions,
            cfg,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("http client"),
            wallet,
            safety_cache: HashMap::new(),
            actions: Default::default(),
            spent_today: 0.0,
            day: String::new(),
        }
    }

    /// 输出一条动作信息：日志模式打到 stdout，仪表盘模式只进缓冲。
    fn say(&mut self, line: String) {
        if !self.cfg.quiet {
            println!("{line}");
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        self.actions.push_back(format!("{ts} {line}"));
        if self.actions.len() > 200 {
            self.actions.pop_front();
        }
    }

    pub fn recent_actions(&self) -> impl DoubleEndedIterator<Item = &String> {
        self.actions.iter()
    }

    pub fn spent_today(&self) -> (f64, f64) {
        (self.spent_today, self.cfg.max_daily_sol)
    }

    pub fn is_live(&self) -> bool {
        self.cfg.live
    }

    /// 仪表盘持仓视图: (mint, raw, 成本SOL, 现值SOL)
    pub fn positions_view(&self) -> Vec<(String, u64, f64, Option<f64>)> {
        let mut v: Vec<_> = self
            .positions
            .iter()
            .filter(|(_, p)| p.raw > 0)
            .map(|(m, p)| (m.clone(), p.raw, p.cost_sol, p.last_value_sol))
            .collect();
        v.sort_by(|a, b| b.2.total_cmp(&a.2));
        v
    }

    fn save_positions(&self) {
        let live: HashMap<&String, &Position> =
            self.positions.iter().filter(|(_, p)| p.raw > 0).collect();
        if let Ok(s) = serde_json::to_string_pretty(&live) {
            let _ = std::fs::write(&self.cfg.positions_path, s);
        }
    }

    /// 止盈/止损巡检：用 Jupiter 报价算每个持仓的可变现 SOL 价值，
    /// 与成本比较，触发则清仓。报价即真实可成交价，天然包含流动性深度。
    pub async fn check_positions(&mut self, rpc: &Rpc) {
        if self.cfg.take_profit <= 0.0 && self.cfg.stop_loss <= 0.0 && !self.cfg.value_positions {
            return;
        }
        let held: Vec<(String, Position)> = self
            .positions
            .iter()
            .filter(|(_, p)| p.raw > 0 && p.cost_sol > 0.0)
            .map(|(m, p)| (m.clone(), p.clone()))
            .collect();
        for (mint, pos) in held {
            let Ok(quote) = self.quote(&mint, WSOL, pos.raw).await else {
                continue;
            };
            let value_sol = quote["outAmount"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0) as f64
                / 1e9;
            if let Some(p) = self.positions.get_mut(&mint) {
                p.last_value_sol = Some(value_sol);
            }
            let ratio = value_sol / pos.cost_sol;
            let reason = if self.cfg.take_profit > 0.0 && ratio >= self.cfg.take_profit {
                "止盈"
            } else if self.cfg.stop_loss > 0.0 && ratio <= self.cfg.stop_loss {
                "止损"
            } else {
                continue;
            };
            self.say(format!(
                "⚡ {reason}触发 {}: 现值 {:.4} SOL / 成本 {:.4} SOL ({:.0}%)",
                short(&mint),
                value_sol,
                pos.cost_sol,
                ratio * 100.0
            ));
            if let Err(e) = self.execute_sell(rpc, &mint, pos.raw, reason).await {
                self.say(format!("✗ {reason}卖出失败: {e}"));
                self.audit(json!({"action": "error", "mint": mint, "error": e.to_string()}));
            }
        }
    }

    /// 买前安全检查（带缓存）。检查失败（网络错误等）视为不安全。
    async fn is_mint_safe(&mut self, rpc: &Rpc, mint: &str) -> bool {
        if let Some(&ok) = self.safety_cache.get(mint) {
            return ok;
        }
        let ok = match crate::safety::check_mint(rpc, mint).await {
            Ok(report) => {
                if !report.is_safe() {
                    self.say(format!(
                        "⚠ 安全检查不通过 {}: {}",
                        short(mint),
                        report.summary()
                    ));
                    self.audit(json!({
                        "action": "skip_buy", "mint": mint,
                        "reason": "safety", "risks": report.risks,
                    }));
                }
                report.is_safe()
            }
            Err(e) => {
                self.say(format!("⚠ 安全检查失败 {}: {e} (按不安全处理)", short(mint)));
                false
            }
        };
        self.safety_cache.insert(mint.to_string(), ok);
        ok
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
            self.say(format!("✗ 跟单失败: {e}"));
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
        if !self.cfg.allow_risky && !self.is_mint_safe(rpc, &ev.mint).await {
            return Ok(()); // 机制风险（可增发/冻结/转账税等），不跟
        }
        self.roll_day();
        if self.spent_today + self.cfg.buy_sol > self.cfg.max_daily_sol {
            self.say(format!(
                "⚠ 跳过跟买 {}: 今日已用 {:.3}/{:.3} SOL",
                ev.token_disp(),
                self.spent_today,
                self.cfg.max_daily_sol
            ));
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
        let disp = ev.token_disp();
        let mode = if self.cfg.live { "[LIVE]" } else { "[paper]" };
        self.say(format!(
            "→ 跟买 {disp}: {:.4} SOL (冲击 {:.2}%) {mode}",
            self.cfg.buy_sol,
            impact * 100.0,
        ));
        if let Some(n) = &self.cfg.notifier {
            n.send(
                &format!("跟买 {disp} {mode}"),
                &format!("{:.4} SOL, 信号源 {}", self.cfg.buy_sol, short(src)),
            );
        }
        self.audit(json!({
            "action": "copy_buy", "src": src, "mint": ev.mint,
            "sol": self.cfg.buy_sol, "quote_out": out_amount,
            "mode": if self.cfg.live { "live" } else { "paper" },
        }));
        if self.cfg.live {
            let sig = self.swap_and_send(rpc, &quote).await?;
            self.say(format!("✓ 买入已确认: {sig}"));
            self.audit(json!({"action": "buy_confirmed", "mint": ev.mint, "sig": sig}));
        }
        self.spent_today += self.cfg.buy_sol;
        let pos = self.positions.entry(ev.mint.clone()).or_default();
        pos.raw += out_amount;
        pos.cost_sol += self.cfg.buy_sol;
        self.save_positions();
        Ok(())
    }

    async fn maybe_copy_sell(&mut self, rpc: &Rpc, src: &str, ev: &WatchEvent) -> Result<()> {
        let held = self.positions.get(&ev.mint).map(|p| p.raw).unwrap_or(0);
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
        self.audit(json!({"action": "copy_sell_signal", "src": src, "mint": ev.mint, "fraction": fraction}));
        self.execute_sell(rpc, &ev.mint, sell_raw, "跟卖").await
    }

    /// 卖出仓位（跟卖/止盈/止损共用）。paper 模式只做账。
    async fn execute_sell(&mut self, rpc: &Rpc, mint: &str, sell_raw: u64, reason: &str) -> Result<()> {
        let held = self.positions.get(mint).map(|p| p.raw).unwrap_or(0);
        let sell_raw = sell_raw.min(held);
        if sell_raw == 0 {
            return Ok(());
        }
        let quote = self.quote(mint, WSOL, sell_raw).await?;
        let out_sol = quote["outAmount"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0) as f64
            / 1e9;
        let fraction = sell_raw as f64 / held as f64;
        let mode = if self.cfg.live { "[LIVE]" } else { "[paper]" };
        self.say(format!(
            "→ {reason} {}: {:.0}% 仓位 ≈ {:.4} SOL {mode}",
            short(mint),
            fraction * 100.0,
            out_sol,
        ));
        if let Some(n) = &self.cfg.notifier {
            n.send(
                &format!("{reason} {} {mode}", short(mint)),
                &format!("{:.0}% 仓位 ≈ {out_sol:.4} SOL", fraction * 100.0),
            );
        }
        let mut entry = json!({
            "action": "sell", "reason": reason, "mint": mint,
            "sell_raw": sell_raw, "est_sol": out_sol,
            "mode": if self.cfg.live { "live" } else { "paper" },
        });
        if self.cfg.live {
            let sig = self.swap_and_send(rpc, &quote).await?;
            self.say(format!("✓ 卖出已确认: {sig}"));
            entry["sig"] = json!(sig);
        }
        let pos = self.positions.get_mut(mint).unwrap();
        let cost_removed = pos.cost_sol * fraction;
        pos.raw -= sell_raw;
        pos.cost_sol -= cost_removed;
        entry["realized_sol"] = json!(out_sol - cost_removed);
        self.audit(entry);
        self.save_positions();
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

    pub fn positions_summary(&self) -> Vec<(String, u64, f64)> {
        self.positions
            .iter()
            .filter(|(_, p)| p.raw > 0)
            .map(|(m, p)| (m.clone(), p.raw, p.cost_sol))
            .collect()
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
