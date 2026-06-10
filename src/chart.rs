//! 筹码结构可视化的计算与渲染：筹码成本分布（筹码峰）、
//! Top 持有人占比瓜分条、持有人盈亏分布。
//!
//! 计算部分产出结构化数据，report.rs 渲染成 ASCII，tui.rs 渲染成带色 Line，
//! 共用本模块的分桶逻辑与 `bar()` 字符画辅助。

use crate::types::{Holder, HolderPnl, human, short};

/// 水平条形：ratio(0-1) → 定宽字符画
pub fn bar(ratio: f64, width: usize) -> String {
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled.min(width)))
}

/// 成本价显示：小额代币要更多小数位
pub fn fmt_price(p: f64) -> String {
    if p >= 1.0 {
        format!("{p:.4}")
    } else if p >= 0.001 {
        format!("{p:.6}")
    } else {
        format!("{p:.9}")
    }
}

// ───────────────────────── 筹码成本分布（筹码峰）─────────────────────────

pub struct CostBucket {
    /// 桶代表价（中点）
    pub price: f64,
    /// 该价位堆积的筹码量
    pub amount: f64,
    /// 该桶成本高于现价 = 套牢
    pub underwater: bool,
}

pub struct CostHistogram {
    pub buckets: Vec<CostBucket>,
    pub max_amount: f64,
    pub profit_amount: f64,
    pub underwater_amount: f64,
    /// 有持仓但成本未知（转入/历史截断）的筹码量
    pub unknown_amount: f64,
    pub price: Option<f64>,
    /// 主力成本区（占比最高的桶）
    pub peak_price: Option<f64>,
    /// 这些数据覆盖了多少个持有人
    pub holders: usize,
}

/// 从已分析持有人的成本均价 + 持仓量构建筹码峰。bins 建议 8-12。
pub fn cost_distribution(pnl: &[HolderPnl], price: Option<f64>, bins: usize) -> Option<CostHistogram> {
    let priced: Vec<(f64, f64)> = pnl
        .iter()
        .filter(|p| p.position > 1e-9)
        .filter_map(|p| p.avg_cost_sol.map(|c| (c, p.position)))
        .filter(|(c, _)| *c > 0.0)
        .collect();
    let unknown_amount: f64 = pnl
        .iter()
        .filter(|p| p.position > 1e-9 && p.avg_cost_sol.is_none())
        .map(|p| p.position)
        .sum();
    if priced.is_empty() {
        return None;
    }

    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for (c, _) in &priced {
        lo = lo.min(*c);
        hi = hi.max(*c);
    }
    // 价差极小时给一点宽度，避免除零
    if (hi - lo) < hi.abs() * 1e-6 {
        hi = lo * 1.0001 + 1e-9;
    }
    let bins = bins.max(1);
    let step = (hi - lo) / bins as f64;
    let mut amounts = vec![0.0f64; bins];
    for (c, amt) in &priced {
        let idx = (((c - lo) / step) as usize).min(bins - 1);
        amounts[idx] += amt;
    }

    let mut buckets = Vec::new();
    let mut max_amount = 0.0f64;
    let mut profit_amount = 0.0;
    let mut underwater_amount = 0.0;
    let (mut peak_amt, mut peak_price) = (0.0f64, None);
    for (i, amt) in amounts.iter().enumerate() {
        let center = lo + step * (i as f64 + 0.5);
        let underwater = price.is_some_and(|px| center > px);
        if *amt > 0.0 {
            if underwater {
                underwater_amount += amt;
            } else if price.is_some() {
                profit_amount += amt;
            }
            if *amt > peak_amt {
                peak_amt = *amt;
                peak_price = Some(center);
            }
        }
        max_amount = max_amount.max(*amt);
        buckets.push(CostBucket {
            price: center,
            amount: *amt,
            underwater,
        });
    }
    // 高价位在上，低价位在下（K 线直觉）
    buckets.reverse();
    Some(CostHistogram {
        buckets,
        max_amount,
        profit_amount,
        underwater_amount,
        unknown_amount,
        price,
        peak_price,
        holders: priced.len(),
    })
}

// ───────────────────────── Top 持有人占比瓜分条 ─────────────────────────

pub struct HolderSlice {
    pub rank: usize,
    pub owner: String,
    pub pct: f64,
    pub label: Option<String>,
    /// 分层符号: 0 巨鲸 1 大户 2 中户 3 散户
    pub tier: u8,
}

pub fn tier_of(pct: f64) -> u8 {
    if pct >= 1.0 {
        0
    } else if pct >= 0.1 {
        1
    } else if pct >= 0.01 {
        2
    } else {
        3
    }
}

pub fn tier_char(tier: u8) -> char {
    match tier {
        0 => '█',
        1 => '▓',
        2 => '▒',
        _ => '░',
    }
}

pub struct HolderShares {
    pub top: Vec<HolderSlice>,
    /// 列出的 top 之外剩余筹码占比
    pub rest_pct: f64,
    /// 已知占比合计（不足 100 的部分是未扫描到的散户）
    pub covered_pct: f64,
}

pub fn holder_shares(holders: &[Holder], top_n: usize) -> HolderShares {
    let top: Vec<HolderSlice> = holders
        .iter()
        .take(top_n)
        .enumerate()
        .map(|(i, h)| HolderSlice {
            rank: i + 1,
            owner: h.owner.clone(),
            pct: h.pct,
            label: h.label.clone(),
            tier: tier_of(h.pct),
        })
        .collect();
    let covered_pct: f64 = holders.iter().map(|h| h.pct).sum();
    let top_pct: f64 = top.iter().map(|s| s.pct).sum();
    HolderShares {
        top,
        rest_pct: (covered_pct - top_pct).max(0.0),
        covered_pct,
    }
}

/// 把瓜分条铺成定宽字符串：每个 top 持有人按占比分配字符，剩余用散户符号。
pub fn shares_bar(shares: &HolderShares, width: usize) -> String {
    let mut s = String::new();
    let mut used = 0usize;
    for slice in &shares.top {
        let n = ((slice.pct / 100.0) * width as f64).round() as usize;
        if n == 0 {
            continue;
        }
        let n = n.min(width - used);
        s.push_str(&tier_char(slice.tier).to_string().repeat(n));
        used += n;
        if used >= width {
            break;
        }
    }
    if used < width {
        s.push_str(&"░".repeat(width - used));
    }
    s
}

// ───────────────────────── 持有人盈亏分布 ─────────────────────────

pub struct PnlRow {
    pub owner: String,
    pub position: f64,
    pub unrealized_sol: f64,
    pub roi_pct: Option<f64>,
}

pub struct PnlDistribution {
    pub rows: Vec<PnlRow>,
    pub max_position: f64,
    pub profit_holders: usize,
    pub loss_holders: usize,
    pub profit_position: f64,
    pub loss_position: f64,
    pub avg_unrealized: f64,
}

pub fn pnl_distribution(pnl: &[HolderPnl]) -> Option<PnlDistribution> {
    let mut rows: Vec<PnlRow> = pnl
        .iter()
        .filter(|p| p.position > 1e-9)
        .filter_map(|p| {
            p.unrealized_sol.map(|u| PnlRow {
                owner: p.owner.clone(),
                position: p.position,
                unrealized_sol: u,
                roi_pct: (p.cost_sol > 0.0).then(|| u / p.cost_sol * 100.0),
            })
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    rows.sort_by(|a, b| b.unrealized_sol.total_cmp(&a.unrealized_sol));
    let max_position = rows.iter().map(|r| r.position).fold(0.0, f64::max);
    let profit_holders = rows.iter().filter(|r| r.unrealized_sol > 0.0).count();
    let loss_holders = rows.iter().filter(|r| r.unrealized_sol < 0.0).count();
    let profit_position: f64 = rows
        .iter()
        .filter(|r| r.unrealized_sol > 0.0)
        .map(|r| r.position)
        .sum();
    let loss_position: f64 = rows
        .iter()
        .filter(|r| r.unrealized_sol < 0.0)
        .map(|r| r.position)
        .sum();
    let avg_unrealized = rows.iter().map(|r| r.unrealized_sol).sum::<f64>() / rows.len() as f64;
    Some(PnlDistribution {
        rows,
        max_position,
        profit_holders,
        loss_holders,
        profit_position,
        loss_position,
        avg_unrealized,
    })
}

/// owner 显示：优先排名标注
pub fn owner_tag(owner: &str, rank: Option<usize>) -> String {
    match rank {
        Some(r) => format!("#{r:<3}{}", short(owner)),
        None => short(owner),
    }
}

/// 给 report 用的简短量级标注
pub fn amount_pct(amount: f64, total: f64) -> String {
    if total > 0.0 {
        format!("{} ({:.1}%)", human(amount), amount / total * 100.0)
    } else {
        human(amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pnl(owner: &str, pos: f64, cost: f64, avg: Option<f64>, unreal: Option<f64>) -> HolderPnl {
        HolderPnl {
            owner: owner.into(),
            position: pos,
            cost_sol: cost,
            avg_cost_sol: avg,
            unrealized_sol: unreal,
            ..Default::default()
        }
    }

    #[test]
    fn cost_histogram_splits_profit_and_underwater() {
        let pnls = vec![
            pnl("A", 1000.0, 1.0, Some(0.001), Some(1.0)),   // 成本低于现价 → 获利
            pnl("B", 2000.0, 4.0, Some(0.002), Some(-0.5)),  // 成本接近现价
            pnl("C", 500.0, 2.0, Some(0.004), Some(-1.0)),   // 成本高于现价 → 套牢
            pnl("D", 300.0, 0.0, None, Some(0.0)),           // 成本未知
        ];
        let h = cost_distribution(&pnls, Some(0.0025), 8).unwrap();
        assert_eq!(h.holders, 3);
        assert!((h.unknown_amount - 300.0).abs() < 1e-9);
        // A(1000) 获利；C(500) 套牢；B(2000) 成本0.002<0.0025 获利
        assert!((h.profit_amount - 3000.0).abs() < 1e-6, "profit={}", h.profit_amount);
        assert!((h.underwater_amount - 500.0).abs() < 1e-6);
        // 桶按高价在上
        assert!(h.buckets.first().unwrap().price > h.buckets.last().unwrap().price);
    }

    #[test]
    fn shares_bar_width_respected() {
        let holders = vec![
            Holder { owner: "A".into(), token_accounts: vec![], balance: 50.0, pct: 50.0, label: None },
            Holder { owner: "B".into(), token_accounts: vec![], balance: 20.0, pct: 20.0, label: Some("Gate".into()) },
        ];
        let s = holder_shares(&holders, 10);
        assert!((s.covered_pct - 70.0).abs() < 1e-9);
        let bar = shares_bar(&s, 20);
        assert_eq!(bar.chars().count(), 20);
    }

    #[test]
    fn pnl_distribution_counts() {
        let pnls = vec![
            pnl("A", 100.0, 1.0, Some(0.01), Some(2.0)),
            pnl("B", 200.0, 1.0, Some(0.01), Some(-1.0)),
            pnl("C", 50.0, 0.0, None, None), // 无浮动盈亏，排除
        ];
        let d = pnl_distribution(&pnls).unwrap();
        assert_eq!(d.profit_holders, 1);
        assert_eq!(d.loss_holders, 1);
        assert_eq!(d.rows.len(), 2);
        assert_eq!(d.rows[0].owner, "A"); // 盈亏降序
    }
}
