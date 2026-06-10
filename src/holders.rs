//! 持有人扫描与筹码分布统计。

use crate::labels::{SYSTEM_PROGRAM, TOKENKEG, label_for};
use crate::rpc::{Rpc, ui_amount};
use crate::types::{Distribution, Holder, TokenInfo};
use anyhow::{Result, bail};
use serde_json::json;
use std::collections::HashMap;

pub async fn fetch_holders(rpc: &Rpc, mint: &str, mode: &str) -> Result<(TokenInfo, Vec<Holder>)> {
    let acct = rpc
        .call("getAccountInfo", json!([mint, {"encoding": "jsonParsed"}]))
        .await?;
    let v = &acct["value"];
    if v.is_null() {
        bail!("找不到 mint 账户: {mint}");
    }
    let program = v["owner"].as_str().unwrap_or_default().to_string();
    if v["data"]["parsed"]["type"].as_str() != Some("mint") {
        bail!("{mint} 不是一个 SPL Token mint 账户");
    }
    let decimals = v["data"]["parsed"]["info"]["decimals"].as_u64().unwrap_or(0) as u8;

    let supply_res = rpc.call("getTokenSupply", json!([mint])).await?;
    let supply = ui_amount(&supply_res["value"]);

    // (token_account, owner, balance)
    let mut accounts: Vec<(String, String, f64)> = Vec::new();
    let mut complete = false;
    if mode != "largest" {
        match fetch_all_accounts(rpc, mint, &program, decimals).await {
            Ok(a) => {
                accounts = a;
                complete = true;
            }
            Err(e) if mode == "full" => return Err(e),
            Err(e) => eprintln!("⚠ getProgramAccounts 不可用（{e}），回退到 Top20 模式。建议使用 Triton 等支持 gPA 的 RPC。"),
        }
    }
    if !complete {
        accounts = fetch_largest(rpc, mint).await?;
    }

    // 按 owner 聚合
    let mut by_owner: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    for (acct, owner, bal) in accounts {
        if bal <= 0.0 {
            continue;
        }
        by_owner.entry(owner).or_default().push((acct, bal));
    }
    let holder_count = by_owner.len();
    let mut holders: Vec<Holder> = by_owner
        .into_iter()
        .map(|(owner, mut accts)| {
            // 余额大的账户排前面（池子价格发现等场景取 first 即最活跃金库）
            accts.sort_by(|a, b| b.1.total_cmp(&a.1));
            let balance: f64 = accts.iter().map(|(_, b)| b).sum();
            Holder {
                pct: if supply > 0.0 { balance / supply * 100.0 } else { 0.0 },
                label: label_for(&owner).map(String::from),
                owner,
                token_accounts: accts.into_iter().map(|(a, _)| a).collect(),
                balance,
            }
        })
        .collect();
    holders.sort_by(|a, b| b.balance.total_cmp(&a.balance));

    annotate_program_owned(rpc, &mut holders).await;

    let token = TokenInfo {
        mint: mint.to_string(),
        program,
        decimals,
        supply,
        holder_count,
        holders_complete: complete,
    };
    Ok((token, holders))
}

/// --owners 模式：只查指定钱包持有的目标代币账户，绕过全量扫描。
pub async fn fetch_specified(
    rpc: &Rpc,
    mint: &str,
    owners: &[String],
) -> Result<(TokenInfo, Vec<Holder>)> {
    let acct = rpc
        .call("getAccountInfo", json!([mint, {"encoding": "jsonParsed"}]))
        .await?;
    let v = &acct["value"];
    if v.is_null() {
        bail!("找不到 mint 账户: {mint}");
    }
    let program = v["owner"].as_str().unwrap_or_default().to_string();
    let decimals = v["data"]["parsed"]["info"]["decimals"].as_u64().unwrap_or(0) as u8;
    let supply_res = rpc.call("getTokenSupply", json!([mint])).await?;
    let supply = ui_amount(&supply_res["value"]);

    let mut holders = Vec::new();
    for owner in owners {
        let res = rpc
            .call(
                "getTokenAccountsByOwner",
                json!([owner, {"mint": mint}, {"encoding": "jsonParsed"}]),
            )
            .await?;
        let mut token_accounts = Vec::new();
        let mut balance = 0.0;
        for item in res["value"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            token_accounts.push(item["pubkey"].as_str().unwrap_or_default().to_string());
            balance += ui_amount(&item["account"]["data"]["parsed"]["info"]["tokenAmount"]);
        }
        if token_accounts.is_empty() {
            eprintln!("⚠ {owner} 没有该代币的账户，跳过");
            continue;
        }
        holders.push(Holder {
            pct: if supply > 0.0 { balance / supply * 100.0 } else { 0.0 },
            label: None,
            owner: owner.clone(),
            token_accounts,
            balance,
        });
    }
    holders.sort_by(|a, b| b.balance.total_cmp(&a.balance));
    let token = TokenInfo {
        mint: mint.to_string(),
        program,
        decimals,
        supply,
        holder_count: holders.len(),
        holders_complete: false,
    };
    Ok((token, holders))
}

/// 全量扫描代币账户。用 base64 + dataSlice 只取每个账户前 72 字节
/// (mint 32 + owner 32 + amount 8)，把响应体积压到 jsonParsed 的 ~1/10。
async fn fetch_all_accounts(
    rpc: &Rpc,
    mint: &str,
    program: &str,
    decimals: u8,
) -> Result<Vec<(String, String, f64)>> {
    use base64::Engine;
    let mut filters = vec![json!({"memcmp": {"offset": 0, "bytes": mint}})];
    if program == TOKENKEG {
        filters.push(json!({"dataSize": 165}));
    }
    let res = rpc
        .call_long(
            "getProgramAccounts",
            json!([program, {
                "encoding": "base64",
                "dataSlice": {"offset": 0, "length": 72},
                "filters": filters,
                "commitment": "confirmed"
            }]),
        )
        .await?;
    let engine = base64::engine::general_purpose::STANDARD;
    let scale = 10f64.powi(decimals as i32);
    let mut out = Vec::new();
    for item in res.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let Some(data_b64) = item["account"]["data"][0].as_str() else {
            continue;
        };
        let Ok(data) = engine.decode(data_b64) else {
            continue;
        };
        if data.len() < 72 {
            continue; // token-2022 的 mint 账户等非 token account 布局
        }
        let owner = bs58::encode(&data[32..64]).into_string();
        let amount = u64::from_le_bytes(data[64..72].try_into().unwrap());
        let addr = item["pubkey"].as_str().unwrap_or_default().to_string();
        out.push((addr, owner, amount as f64 / scale));
    }
    Ok(out)
}

async fn fetch_largest(rpc: &Rpc, mint: &str) -> Result<Vec<(String, String, f64)>> {
    let res = rpc.call("getTokenLargestAccounts", json!([mint])).await?;
    let entries = res["value"].as_array().cloned().unwrap_or_default();
    let addrs: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["address"].as_str())
        .collect();
    if addrs.is_empty() {
        bail!("getTokenLargestAccounts 没有返回任何账户");
    }
    let multi = rpc
        .call(
            "getMultipleAccounts",
            json!([addrs, {"encoding": "jsonParsed"}]),
        )
        .await?;
    let infos = multi["value"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for (e, info) in entries.iter().zip(infos.iter()) {
        let addr = e["address"].as_str().unwrap_or_default().to_string();
        let bal = ui_amount(e);
        let owner = info["data"]["parsed"]["info"]["owner"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        out.push((addr, owner, bal));
    }
    Ok(out)
}

/// 检查头部持有人钱包是否由程序持有（基本可判定为池子/合约金库），打上标签。
async fn annotate_program_owned(rpc: &Rpc, holders: &mut [Holder]) {
    let top: Vec<String> = holders
        .iter()
        .take(100)
        .filter(|h| h.label.is_none())
        .map(|h| h.owner.clone())
        .collect();
    for chunk in top.chunks(100) {
        let Ok(res) = rpc
            .call(
                "getMultipleAccounts",
                json!([chunk, {"encoding": "base64", "dataSlice": {"offset": 0, "length": 0}}]),
            )
            .await
        else {
            return;
        };
        let values = res["value"].as_array().cloned().unwrap_or_default();
        for (addr, val) in chunk.iter().zip(values.iter()) {
            let owner_prog = val["owner"].as_str();
            if owner_prog.is_some() && owner_prog != Some(SYSTEM_PROGRAM) {
                if let Some(h) = holders.iter_mut().find(|h| &h.owner == addr) {
                    h.label = Some("合约/池子".into());
                }
            }
        }
    }
}

pub fn distribution(token: &TokenInfo, holders: &[Holder]) -> Distribution {
    let supply = token.supply.max(f64::MIN_POSITIVE);
    let sum_pct = |n: usize| -> f64 {
        holders.iter().take(n).map(|h| h.balance).sum::<f64>() / supply * 100.0
    };
    let hhi = holders
        .iter()
        .map(|h| {
            let s = h.balance / supply * 100.0;
            s * s
        })
        .sum::<f64>();

    let mut buckets: Vec<(String, usize, f64)> = vec![
        ("巨鲸 ≥1%".into(), 0, 0.0),
        ("大户 0.1%-1%".into(), 0, 0.0),
        ("中户 0.01%-0.1%".into(), 0, 0.0),
        ("散户 <0.01%".into(), 0, 0.0),
    ];
    for h in holders {
        let i = if h.pct >= 1.0 {
            0
        } else if h.pct >= 0.1 {
            1
        } else if h.pct >= 0.01 {
            2
        } else {
            3
        };
        buckets[i].1 += 1;
        buckets[i].2 += h.pct;
    }

    Distribution {
        top1_pct: sum_pct(1),
        top10_pct: sum_pct(10),
        top20_pct: sum_pct(20),
        top100_pct: sum_pct(100),
        hhi,
        buckets,
    }
}
