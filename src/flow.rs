//! 资金流向溯源：找出每个持有人钱包最早的 SOL 入金来源，
//! 并跨持有人聚合出共享资金来源（关联钱包集群）。

use crate::labels::label_for;
use crate::rpc::Rpc;
use crate::types::{Cluster, FundingSource, HolderFlow};
use anyhow::Result;
use futures::future::join_all;
use serde_json::Value;
use std::collections::HashMap;

/// 翻页查找钱包最早历史时的签名数量上限（防止巨鲸钱包翻几万页）
const SIG_HISTORY_CAP: usize = 5000;

pub async fn trace_funding(rpc: &Rpc, owner: &str, scan_limit: usize) -> Result<HolderFlow> {
    let sigs = rpc.signatures(owner, SIG_HISTORY_CAP).await?;
    let reached_genesis = sigs.len() < SIG_HISTORY_CAP;
    // 签名是最新在前，资金来源看最早的交易
    let oldest: Vec<&Value> = sigs
        .iter()
        .rev()
        .filter(|s| s["err"].is_null())
        .take(scan_limit)
        .collect();

    let txs = join_all(
        oldest
            .iter()
            .filter_map(|s| s["signature"].as_str())
            .map(|sig| rpc.transaction(sig)),
    )
    .await;

    let mut by_source: HashMap<String, FundingSource> = HashMap::new();
    for tx in txs.into_iter().flatten() {
        let time = tx["blockTime"].as_i64();
        for (source, sol) in incoming_sol(&tx, owner) {
            let e = by_source
                .entry(source.clone())
                .or_insert_with(|| FundingSource {
                    label: label_for(&source).map(String::from),
                    source,
                    total_sol: 0.0,
                    count: 0,
                    first_time: time,
                });
            e.total_sol += sol;
            e.count += 1;
            if time.is_some() && (e.first_time.is_none() || time < e.first_time) {
                e.first_time = time;
            }
        }
    }

    let mut sources: Vec<FundingSource> = by_source.into_values().collect();
    sources.sort_by(|a, b| b.total_sol.total_cmp(&a.total_sol));
    // 过滤灰尘入金（垃圾空投/广告转账常见 0.000x SOL）；全是灰尘时保留最大的几个
    let significant: Vec<FundingSource> = sources
        .iter()
        .filter(|s| s.total_sol >= 0.01)
        .cloned()
        .collect();
    if !significant.is_empty() {
        sources = significant;
    } else {
        sources.truncate(3);
    }
    Ok(HolderFlow {
        owner: owner.to_string(),
        sources,
        scanned_txs: oldest.len(),
        reached_genesis,
    })
}

/// 提取一笔交易中转入 `owner` 的 SOL（system program 的 transfer/createAccount，含内层指令）。
fn incoming_sol(tx: &Value, owner: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    collect_sol_transfers(&tx["transaction"]["message"]["instructions"], owner, &mut out);
    for inner in tx["meta"]["innerInstructions"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
    {
        collect_sol_transfers(&inner["instructions"], owner, &mut out);
    }
    out
}

fn collect_sol_transfers(instrs: &Value, owner: &str, out: &mut Vec<(String, f64)>) {
    let Some(arr) = instrs.as_array() else { return };
    for ins in arr {
        if ins["program"].as_str() != Some("system") {
            continue;
        }
        let parsed = &ins["parsed"];
        let info = &parsed["info"];
        let (src, dst, lamports) = match parsed["type"].as_str().unwrap_or("") {
            "transfer" | "transferWithSeed" => (
                info["source"].as_str(),
                info["destination"].as_str(),
                info["lamports"].as_u64(),
            ),
            "createAccount" | "createAccountWithSeed" => (
                info["source"].as_str(),
                info["newAccount"].as_str(),
                info["lamports"].as_u64(),
            ),
            _ => continue,
        };
        if dst == Some(owner) && src != Some(owner) {
            if let (Some(s), Some(l)) = (src, lamports) {
                out.push((s.to_string(), l as f64 / 1e9));
            }
        }
    }
}

/// 资金来源若本身就是 Top 持有人，标注出来（大户间互转是强关联信号）。
pub fn annotate_holder_sources(flows: &mut [HolderFlow], holders: &[crate::types::Holder]) {
    use std::collections::HashMap as Map;
    let rank: Map<&str, usize> = holders
        .iter()
        .enumerate()
        .map(|(i, h)| (h.owner.as_str(), i + 1))
        .collect();
    for f in flows {
        for s in &mut f.sources {
            if let Some(r) = rank.get(s.source.as_str()) {
                let tag = format!("持有人#{r}");
                s.label = Some(match &s.label {
                    Some(l) => format!("{l}·{tag}"),
                    None => tag,
                });
            }
        }
    }
}

/// 跨持有人找共享资金来源。交易所热钱包给很多人打过钱，关联性弱；
/// 非交易所的共享来源是强关联信号（很可能是同一控制人/分发钱包）。
pub fn find_clusters(flows: &[HolderFlow]) -> Vec<Cluster> {
    struct Acc {
        label: Option<String>,
        holders: Vec<String>,
        total: f64,
        times: Vec<i64>,
    }
    let mut map: HashMap<&str, Acc> = HashMap::new();
    for f in flows {
        for s in &f.sources {
            let e = map.entry(s.source.as_str()).or_insert_with(|| Acc {
                label: s.label.clone(),
                holders: Vec::new(),
                total: 0.0,
                times: Vec::new(),
            });
            if !e.holders.contains(&f.owner) {
                e.holders.push(f.owner.clone());
            }
            e.total += s.total_sol;
            e.times.extend(s.first_time);
        }
    }
    let mut clusters: Vec<Cluster> = map
        .into_iter()
        .filter_map(|(source, acc)| {
            if acc.holders.len() < 2 {
                return None;
            }
            let span = match (acc.times.iter().min(), acc.times.iter().max()) {
                (Some(min), Some(max)) => Some(max - min),
                _ => None,
            };
            // 金额够大算关联；金额小但入金时间集中在 6 小时内也是强信号
            // （钱包农场用同一钱包批量注入开户租金）。真正的垃圾空投通常
            // 持续数天扫所有地址，时间跨度大，被这里排除。
            let coordinated = span.is_some_and(|s| s <= 6 * 3600);
            if acc.total < 0.05 && !coordinated {
                return None;
            }
            Some(Cluster {
                source: source.to_string(),
                label: acc.label,
                holders: acc.holders,
                total_sol: acc.total,
                time_span_secs: span,
            })
        })
        .collect();
    // 非交易所来源（强关联）排前面，按覆盖持有人数量降序
    clusters.sort_by(|a, b| {
        let a_ex = a.label.as_deref().is_some_and(crate::labels::is_exchange);
        let b_ex = b.label.as_deref().is_some_and(crate::labels::is_exchange);
        a_ex.cmp(&b_ex)
            .then(b.holders.len().cmp(&a.holders.len()))
            .then(b.total_sol.total_cmp(&a.total_sol))
    });
    clusters
}
