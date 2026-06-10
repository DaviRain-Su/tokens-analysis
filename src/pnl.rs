//! 持有人盈亏分析：从交易历史还原买卖账本，计算成本均价与已实现/浮动盈亏。
//!
//! 核心思路：对每笔交易比较该钱包的 pre/post 余额 ——
//! 代币增加且 SOL 减少 = 买入；代币减少且 SOL 增加 = 卖出；
//! 没有对手资产变化 = 普通转账。成交价直接由两边变化量之比得出，
//! 无需解析具体 DEX 的指令格式（对 Raydium/Pump/Jupiter 等通用）。

use crate::labels::{USDC, USDT, WSOL};
use crate::rpc::{Rpc, ui_amount};
use crate::types::{HolderPnl, Side, TradeEvent};
use anyhow::Result;
use futures::future::join_all;
use serde_json::Value;

/// 小于该值的 SOL 变化视为手续费/租金噪音，不当作交易对价
const SOL_NOISE: f64 = 0.001;
const USD_NOISE: f64 = 0.01;
const DUST: f64 = 1e-9;

pub async fn analyze_holder(
    rpc: &Rpc,
    owner: &str,
    token_accounts: &[String],
    mint: &str,
    current_balance: f64,
    tx_limit: usize,
) -> Result<HolderPnl> {
    // 用代币账户（而不是钱包）拉签名，只命中与该代币相关的交易
    let mut sigs: Vec<(String, Option<i64>)> = Vec::new();
    for acct in token_accounts {
        for s in rpc.signatures(acct, tx_limit).await? {
            if !s["err"].is_null() {
                continue;
            }
            if let Some(sig) = s["signature"].as_str() {
                sigs.push((sig.to_string(), s["blockTime"].as_i64()));
            }
        }
    }
    sigs.sort_by(|a, b| a.0.cmp(&b.0));
    sigs.dedup_by(|a, b| a.0 == b.0);
    sigs.sort_by(|a, b| b.1.cmp(&a.1));
    let partial_history = sigs.len() >= tx_limit;
    sigs.truncate(tx_limit);

    let txs = join_all(sigs.iter().map(|(sig, _)| rpc.transaction(sig))).await;
    let mut events: Vec<TradeEvent> = txs
        .into_iter()
        .filter_map(|t| t.ok())
        .filter_map(|t| parse_event(&t, owner, mint))
        .collect();
    events.sort_by_key(|e| e.time);

    Ok(build_ledger(owner, events, current_balance, partial_history))
}

fn build_ledger(
    owner: &str,
    events: Vec<TradeEvent>,
    current_balance: f64,
    partial_history: bool,
) -> HolderPnl {
    let mut p = HolderPnl {
        owner: owner.to_string(),
        partial_history,
        ..Default::default()
    };

    // 历史被截断时，把扫描窗口之前已有的仓位当作"成本未知"的期初余额
    let net_delta: f64 = events
        .iter()
        .map(|e| match e.side {
            Side::Buy | Side::TransferIn => e.token_amount,
            Side::Sell | Side::TransferOut => -e.token_amount,
        })
        .sum();
    let baseline = (current_balance - net_delta).max(0.0);
    let mut pos = baseline;
    let mut cost = 0.0f64;
    if baseline > DUST {
        p.has_unknown_cost = true;
    }

    for e in &events {
        let amt = e.token_amount;
        match e.side {
            Side::Buy => {
                pos += amt;
                p.bought_tokens += amt;
                if e.sol_amount > 0.0 {
                    cost += e.sol_amount;
                    p.sol_spent += e.sol_amount;
                } else {
                    // 稳定币买入：无法折算 SOL 成本，仅记数量
                    p.usd_spent += e.usd_amount;
                    p.has_unknown_cost = true;
                }
            }
            Side::Sell => {
                let sell = amt.min(pos);
                let avg = if pos > DUST { cost / pos } else { 0.0 };
                if e.sol_amount > 0.0 {
                    p.sol_received += e.sol_amount;
                    p.realized_sol += e.sol_amount - sell * avg;
                } else {
                    p.usd_received += e.usd_amount;
                }
                cost -= sell * avg;
                pos -= sell;
                p.sold_tokens += amt;
            }
            Side::TransferIn => {
                pos += amt;
                p.transfer_in += amt;
                p.has_unknown_cost = true;
            }
            Side::TransferOut => {
                let out = amt.min(pos);
                let avg = if pos > DUST { cost / pos } else { 0.0 };
                cost -= out * avg;
                pos -= out;
                p.transfer_out += amt;
            }
        }
        if pos < DUST {
            pos = 0.0;
            cost = 0.0;
        }
    }

    p.position = pos;
    p.cost_sol = cost.max(0.0);
    p.avg_cost_sol = (pos > DUST && cost > 0.0).then(|| cost / pos);
    p.first_time = events.iter().filter_map(|e| e.time).min();
    p.last_time = events.iter().filter_map(|e| e.time).max();
    p.events = events;
    p
}

/// 从一笔 jsonParsed 交易中提取该钱包对目标代币的买卖/转账事件。
pub fn parse_event(tx: &Value, owner: &str, mint: &str) -> Option<TradeEvent> {
    let meta = tx.get("meta")?;
    if !meta["err"].is_null() {
        return None;
    }
    let token_delta = token_balance_delta(meta, owner, mint);
    if token_delta.abs() < DUST {
        return None;
    }

    let keys = tx["transaction"]["message"]["accountKeys"].as_array()?;
    let idx = keys
        .iter()
        .position(|k| k["pubkey"].as_str() == Some(owner));
    let mut sol_delta = 0.0f64;
    if let Some(i) = idx {
        let pre = meta["preBalances"][i].as_i64().unwrap_or(0);
        let post = meta["postBalances"][i].as_i64().unwrap_or(0);
        sol_delta = (post - pre) as f64 / 1e9;
        if i == 0 {
            // 自己是 fee payer 时把手续费加回来，避免污染成交价
            sol_delta += meta["fee"].as_u64().unwrap_or(0) as f64 / 1e9;
        }
    }
    sol_delta += token_balance_delta(meta, owner, WSOL);
    let usd_delta = token_balance_delta(meta, owner, USDC) + token_balance_delta(meta, owner, USDT);

    let (side, sol_amount, usd_amount) = if token_delta > 0.0 {
        if sol_delta < -SOL_NOISE {
            (Side::Buy, -sol_delta, 0.0)
        } else if usd_delta < -USD_NOISE {
            (Side::Buy, 0.0, -usd_delta)
        } else {
            (Side::TransferIn, 0.0, 0.0)
        }
    } else if sol_delta > SOL_NOISE {
        (Side::Sell, sol_delta, 0.0)
    } else if usd_delta > USD_NOISE {
        (Side::Sell, 0.0, usd_delta)
    } else {
        (Side::TransferOut, 0.0, 0.0)
    };

    let token_amount = token_delta.abs();
    Some(TradeEvent {
        signature: tx["transaction"]["signatures"][0]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        time: tx["blockTime"].as_i64(),
        side,
        token_amount,
        sol_amount,
        usd_amount,
        price_sol: (sol_amount > 0.0 && token_amount > DUST).then(|| sol_amount / token_amount),
    })
}

fn token_balance_delta(meta: &Value, owner: &str, mint: &str) -> f64 {
    let sum = |key: &str| -> f64 {
        meta[key]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|b| {
                        b["owner"].as_str() == Some(owner) && b["mint"].as_str() == Some(mint)
                    })
                    .map(|b| ui_amount(&b["uiTokenAmount"]))
                    .sum()
            })
            .unwrap_or(0.0)
    };
    sum("postTokenBalances") - sum("preTokenBalances")
}

/// 在所有持有人的事件里找最近一笔 SOL 计价成交，作为"最新价格"。
pub fn latest_price(pnls: &[HolderPnl]) -> (Option<f64>, Option<i64>) {
    let mut best: Option<(i64, f64)> = None;
    for p in pnls {
        for e in &p.events {
            if let (Some(t), Some(px)) = (e.time, e.price_sol) {
                if best.is_none_or(|(bt, _)| t > bt) {
                    best = Some((t, px));
                }
            }
        }
    }
    (best.map(|(_, px)| px), best.map(|(t, _)| t))
}

/// 拿到最新价后回填浮动盈亏。成本完全未知（纯转入仓位）时不计算，
/// 避免把整个仓位市值当成利润。
pub fn fill_unrealized(pnls: &mut [HolderPnl], last_price: Option<f64>) {
    let Some(px) = last_price else { return };
    for p in pnls {
        if p.position <= DUST {
            p.unrealized_sol = Some(0.0);
        } else if p.cost_sol > 0.0 {
            p.unrealized_sol = Some(p.position * px - p.cost_sol);
        }
    }
}

/// 从 AMM 池子金库的最近交易里发现最新成交价：
/// 池子的 token/wSOL 余额变化量之比就是成交价，与具体 DEX 协议无关。
pub async fn pool_price(
    rpc: &Rpc,
    pool_owner: &str,
    token_accounts: &[String],
    mint: &str,
) -> (Option<f64>, Option<i64>) {
    let Some(acct) = token_accounts.first() else {
        return (None, None);
    };
    let Ok(sigs) = rpc.signatures(acct, 25).await else {
        return (None, None);
    };
    let txs = join_all(
        sigs.iter()
            .filter(|s| s["err"].is_null())
            .filter_map(|s| s["signature"].as_str())
            .map(|sig| rpc.transaction(sig)),
    )
    .await;
    let mut best: Option<(i64, f64)> = None;
    for tx in txs.into_iter().flatten() {
        if let Some(e) = parse_event(&tx, pool_owner, mint) {
            if let (Some(t), Some(px)) = (e.time, e.price_sol) {
                if best.is_none_or(|(bt, _)| t > bt) {
                    best = Some((t, px));
                }
            }
        }
    }
    (best.map(|(_, px)| px), best.map(|(t, _)| t))
}

/// 从 Raydium SOL/USDC 池推导 SOL/USD 汇率：
/// 该池金库由 Raydium V4 权限账户持有，其 USDC 与 wSOL 的余额变化量之比
/// 就是成交汇率（parse_event 以 USDC 为目标 mint 时 price_sol = SOL/USDC，取倒数）。
pub async fn sol_usd_price(rpc: &Rpc) -> Option<f64> {
    const RAYDIUM_AUTH: &str = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";
    let res = rpc
        .call(
            "getTokenAccountsByOwner",
            serde_json::json!([RAYDIUM_AUTH, {"mint": USDC}, {"encoding": "jsonParsed"}]),
        )
        .await
        .ok()?;
    // 取余额最大的 USDC 金库（即最深的 USDC 交易对，通常是 SOL/USDC）
    let vault = res["value"]
        .as_array()?
        .iter()
        .max_by(|a, b| {
            let bal = |v: &&Value| {
                ui_amount(&v["account"]["data"]["parsed"]["info"]["tokenAmount"])
            };
            bal(a).total_cmp(&bal(b))
        })?["pubkey"]
        .as_str()?
        .to_string();
    let sigs = rpc.signatures(&vault, 15).await.ok()?;
    let txs = join_all(
        sigs.iter()
            .filter(|s| s["err"].is_null())
            .filter_map(|s| s["signature"].as_str())
            .map(|sig| rpc.transaction(sig)),
    )
    .await;
    let mut best: Option<(i64, f64)> = None;
    for tx in txs.into_iter().flatten() {
        if let Some(e) = parse_event(&tx, RAYDIUM_AUTH, USDC) {
            if let (Some(t), Some(px)) = (e.time, e.price_sol) {
                if px > 0.0 && best.is_none_or(|(bt, _)| t > bt) {
                    best = Some((t, px));
                }
            }
        }
    }
    // price_sol = SOL per USDC，倒数即 USD per SOL
    best.map(|(_, px)| 1.0 / px)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWNER: &str = "OwnerWallet1111111111111111111111111111111";
    const MINT: &str = "Mint11111111111111111111111111111111111111";

    /// 构造一笔 jsonParsed 交易：owner 的代币与 SOL 余额发生变化
    fn make_tx(
        time: i64,
        token_pre: f64,
        token_post: f64,
        lamports_pre: u64,
        lamports_post: u64,
        fee: u64,
    ) -> serde_json::Value {
        let tb = |amt: f64| {
            json!([{
                "accountIndex": 1,
                "mint": MINT,
                "owner": OWNER,
                "uiTokenAmount": {"uiAmount": amt, "amount": format!("{}", (amt*1e6) as u64), "decimals": 6}
            }])
        };
        json!({
            "blockTime": time,
            "transaction": {
                "signatures": ["sig"],
                "message": {"accountKeys": [{"pubkey": OWNER}, {"pubkey": "other"}]}
            },
            "meta": {
                "err": null,
                "fee": fee,
                "preBalances": [lamports_pre, 0],
                "postBalances": [lamports_post, 0],
                "preTokenBalances": tb(token_pre),
                "postTokenBalances": tb(token_post),
            }
        })
    }

    #[test]
    fn buy_sell_classification_and_price() {
        // 花 1 SOL (+5000 lamports 手续费) 买 1000 个币
        let buy = make_tx(100, 0.0, 1000.0, 2_000_000_000, 999_995_000, 5000);
        let e = parse_event(&buy, OWNER, MINT).unwrap();
        assert_eq!(e.side, Side::Buy);
        assert!((e.sol_amount - 1.0).abs() < 1e-9, "手续费应被剔除, got {}", e.sol_amount);
        assert!((e.price_sol.unwrap() - 0.001).abs() < 1e-12);

        // 卖 500 个币收到 1 SOL
        let sell = make_tx(200, 1000.0, 500.0, 999_995_000, 1_999_990_000, 5000);
        let e = parse_event(&sell, OWNER, MINT).unwrap();
        assert_eq!(e.side, Side::Sell);
        assert!((e.sol_amount - 1.0).abs() < 1e-9);

        // 纯转入：代币增加，SOL 不变
        let tin = make_tx(300, 500.0, 700.0, 1_999_990_000, 1_999_985_000, 5000);
        let e = parse_event(&tin, OWNER, MINT).unwrap();
        assert_eq!(e.side, Side::TransferIn);
    }

    #[test]
    fn ledger_realized_and_cost() {
        let events = vec![
            // 1 SOL 买 1000 个，均价 0.001
            parse_event(&make_tx(100, 0.0, 1000.0, 3_000_000_000, 1_999_995_000, 5000), OWNER, MINT).unwrap(),
            // 卖 500 个收 1 SOL（成本 0.5）→ 已实现 +0.5
            parse_event(&make_tx(200, 1000.0, 500.0, 1_999_995_000, 2_999_990_000, 5000), OWNER, MINT).unwrap(),
        ];
        let p = build_ledger(OWNER, events, 500.0, false);
        assert!((p.realized_sol - 0.5).abs() < 1e-9, "realized={}", p.realized_sol);
        assert!((p.position - 500.0).abs() < 1e-6);
        assert!((p.cost_sol - 0.5).abs() < 1e-9);
        assert!((p.avg_cost_sol.unwrap() - 0.001).abs() < 1e-12);
        assert!(!p.has_unknown_cost);

        // 浮动盈亏：现价 0.002 → 500 * 0.002 - 0.5 = +0.5
        let mut v = vec![p];
        fill_unrealized(&mut v, Some(0.002));
        assert!((v[0].unrealized_sol.unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn truncated_history_baseline() {
        // 只看到一笔卖出，但当前余额 300：期初应推算为 800，成本未知
        let events = vec![
            parse_event(&make_tx(200, 800.0, 300.0, 1_000_000_000, 1_999_995_000, 5000), OWNER, MINT).unwrap(),
        ];
        let p = build_ledger(OWNER, events, 300.0, true);
        assert!((p.position - 300.0).abs() < 1e-6);
        assert!(p.has_unknown_cost);
        // 期初成本 0，卖出 500 收 1 SOL 全算已实现
        assert!((p.realized_sol - 1.0).abs() < 1e-9);
    }
}
