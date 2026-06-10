//! 纯文本报告输出（--no-tui 或非终端环境）。

use crate::types::{Analysis, HolderPnl, fmt_time, human, short};

pub fn print(a: &Analysis) {
    let t = &a.token;
    println!("══════════════════════════════════════════════════════════");
    println!("  SPL Token 分析报告");
    println!("══════════════════════════════════════════════════════════");
    if let Some(sym) = &t.symbol {
        println!("代币:        {sym}");
    }
    println!("Mint:        {}", t.mint);
    println!("Program:     {}", t.program);
    println!("总供应量:    {} (decimals={})", human(t.supply), t.decimals);
    if t.holders_complete {
        println!("持有人数量:  {}", t.holder_count);
    } else {
        println!("持有人数量:  ≥{} (仅 Top20 模式，RPC 不支持全量扫描)", t.holder_count);
    }
    if let Some(px) = a.last_price_sol {
        let usd = a
            .sol_usd
            .map(|r| format!("  ≈ ${:.8}", px * r))
            .unwrap_or_default();
        println!(
            "最新成交价:  {:.10} SOL{usd}  ({})",
            px,
            fmt_time(a.last_price_time)
        );
    }
    if let Some(r) = a.sol_usd {
        println!("SOL/USD:     ${r:.2}");
    }
    if let Some(s) = &a.safety {
        println!("安全检查:    {}", s.summary());
    }

    println!("\n── 筹码集中度 ─────────────────────────────────────────────");
    let d = &a.dist;
    println!(
        "Top1: {:.2}%   Top10: {:.2}%   Top20: {:.2}%   Top100: {:.2}%   HHI: {:.0}",
        d.top1_pct, d.top10_pct, d.top20_pct, d.top100_pct, d.hhi
    );
    for (name, count, pct) in &d.buckets {
        println!("  {name:<16} {count:>8} 个地址   占供应 {pct:.2}%");
    }

    println!("\n── Top 持有人 ─────────────────────────────────────────────");
    println!(
        "{:<4}{:<14}{:>12}{:>8}  {}",
        "#", "地址", "余额", "占比%", "标签"
    );
    for (i, h) in a.holders.iter().take(20).enumerate() {
        println!(
            "{:<4}{:<14}{:>12}{:>8.2}  {}",
            i + 1,
            short(&h.owner),
            human(h.balance),
            h.pct,
            h.label.as_deref().unwrap_or("")
        );
    }

    println!("\n── 持有人盈亏 (SOL 计价) ──────────────────────────────────");
    println!(
        "{:<14}{:>10}{:>10}{:>10}{:>10}{:>14}{:>10}{:>10}{:>6}  {}",
        "地址", "买入量", "卖出量", "转入量", "转出量", "均价(SOL)", "已实现", "浮动", "评分", "状态"
    );
    for p in &a.pnl {
        let avg = p
            .avg_cost_sol
            .map(|v| format!("{v:.10}"))
            .unwrap_or_else(|| "-".into());
        let unreal = p
            .unrealized_sol
            .map(|v| format!("{v:+.3}"))
            .unwrap_or_else(|| "-".into());
        let score = crate::pnl::smart_score(p, a.sol_usd)
            .map(|s| format!("{s:.0}"))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<14}{:>10}{:>10}{:>10}{:>10}{:>14}{:>10}{:>10}{:>6}  {}",
            short(&p.owner),
            human(p.bought_tokens),
            human(p.sold_tokens),
            human(p.transfer_in),
            human(p.transfer_out),
            avg,
            format!("{:+.3}", p.realized_sol),
            unreal,
            score,
            status_text(p)
        );
        if p.usd_spent > 0.0 || p.usd_received > 0.0 {
            println!(
                "              └ 稳定币: 买入 ${:.2} / 卖出 ${:.2} (未计入 SOL 成本)",
                p.usd_spent, p.usd_received
            );
        }
    }

    println!("\n── 资金来源 (最早入金) ────────────────────────────────────");
    for f in &a.flows {
        let g = if f.reached_genesis { "✓完整" } else { "~部分" };
        println!("{} ({}, 扫描{}笔):", short(&f.owner), g, f.scanned_txs);
        if f.sources.is_empty() {
            println!("    (未发现 SOL 入金)");
        }
        for s in f.sources.iter().take(5) {
            println!(
                "    ← {:<14} {:>11.4} SOL ×{:<3} {} {}",
                short(&s.source),
                s.total_sol,
                s.count,
                fmt_time(s.first_time),
                s.label.as_deref().unwrap_or("")
            );
            for u in a.upstream.get(&s.source).into_iter().flatten().take(3) {
                println!(
                    "       ↖ {:<14} {:>9.4} SOL ×{:<3} {} {}",
                    short(&u.source),
                    u.total_sol,
                    u.count,
                    fmt_time(u.first_time),
                    u.label.as_deref().unwrap_or("")
                );
            }
        }
    }

    if !a.transfer_links.is_empty() {
        println!("\n── 代币互转 (已分析持有人 ↔ 其他钱包) ─────────────────────");
        let rank: std::collections::HashMap<&str, usize> = a
            .holders
            .iter()
            .enumerate()
            .map(|(i, h)| (h.owner.as_str(), i + 1))
            .collect();
        let tag = |addr: &str| -> String {
            match rank.get(addr) {
                Some(r) => format!("{}(#{r})", short(addr)),
                None => short(addr),
            }
        };
        for l in a.transfer_links.iter().take(15) {
            println!(
                "  {} → {}  {:>10} ×{:<3} {}",
                tag(&l.from),
                tag(&l.to),
                human(l.tokens),
                l.count,
                fmt_time(l.last_time)
            );
        }
    }

    if let Some(d) = &a.snapshot_diff {
        println!("\n── 筹码迁移 (对比 {} 快照) ──────────────", fmt_time(Some(d.base_time)));
        println!(
            "新进持有人 {} 个, 清仓退出 {} 个, 余额变化 {} 个",
            d.new_holders,
            d.exited_holders,
            d.changes.len()
        );
        for (owner, old, new) in d.changes.iter().take(15) {
            let delta = new - old;
            let dir = if *old == 0.0 {
                "🆕新进"
            } else if *new == 0.0 {
                "💨清仓"
            } else if delta > 0.0 {
                "↗加仓"
            } else {
                "↘减仓"
            };
            println!(
                "  {} {:<14} {:>12} → {:<12} ({}{})",
                dir,
                short(owner),
                human(*old),
                human(*new),
                if delta > 0.0 { "+" } else { "" },
                human(delta)
            );
        }
    }

    println!("\n── 关联资金集群 ───────────────────────────────────────────");
    if a.clusters.is_empty() {
        println!("(未发现共享资金来源)");
    }
    for c in &a.clusters {
        let mut kind = match &c.label {
            Some(l) if crate::labels::is_exchange(l) => format!("[{l} 交易所·弱关联]"),
            Some(l) => format!("[{l}]"),
            None => "[私人钱包·强关联]".into(),
        };
        if c.time_span_secs.is_some_and(|s| s <= 6 * 3600) {
            kind.push_str(" [⏱同时段集中入金]");
        }
        println!(
            "来源 {} {} → {} 个持有人, 共 {:.4} SOL",
            short(&c.source),
            kind,
            c.holders.len(),
            c.total_sol
        );
        println!(
            "    {}",
            c.holders
                .iter()
                .map(|h| short(h))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!();
}

pub fn status_text(p: &HolderPnl) -> String {
    let prefix = if p.has_unknown_cost || p.partial_history {
        "~"
    } else {
        ""
    };
    if p.position < 1e-9 && p.sold_tokens > 0.0 {
        return format!("{prefix}已清仓");
    }
    match p.unrealized_sol {
        Some(v) if v > 0.0 => format!("{prefix}浮盈"),
        Some(v) if v < 0.0 => format!("{prefix}浮亏"),
        _ => format!("{prefix}未知"),
    }
}
