//! tokens-analysis: Solana SPL Token 筹码结构与资金流向分析工具。

mod flow;
mod holders;
mod labels;
mod pnl;
mod report;
mod rpc;
mod tui;
mod types;

use anyhow::Result;
use clap::Parser;
use futures::future::join_all;
use indicatif::{ProgressBar, ProgressStyle};
use rpc::Rpc;
use std::io::IsTerminal;
use types::{Analysis, Holder};

#[derive(Parser)]
#[command(
    name = "tokens-analysis",
    about = "Solana SPL Token 筹码结构与资金流向分析工具",
    after_help = "示例:\n  tokens-analysis <MINT> --rpc https://<your-endpoint>.rpcpool.com/<token>\n\n\
                  建议使用支持 getProgramAccounts 的 RPC（如 Triton One, https://docs.triton.one），\n\
                  公共节点会回退到 Top20 持有人模式且容易限流。"
)]
struct Args {
    /// SPL Token 的 mint 地址
    mint: String,

    /// RPC 端点。未指定时依次取 env SOLANA_RPC_URL → Solana CLI 配置 → 公共节点
    #[arg(long, env = "SOLANA_RPC_URL")]
    rpc: Option<String>,

    /// 深度分析（盈亏+资金溯源）的持有人数量
    #[arg(long, default_value_t = 10)]
    top: usize,

    /// 每个持有人最多扫描的代币交易数
    #[arg(long, default_value_t = 60)]
    tx_limit: usize,

    /// 资金溯源时扫描的最早交易数
    #[arg(long, default_value_t = 25)]
    funding_scan: usize,

    /// 资金溯源跳数：1 = 只看持有人的直接入金，2 = 继续追上游来源的来源
    #[arg(long, default_value_t = 1)]
    hops: usize,

    /// RPC 并发请求数
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// 不进入 TUI，直接打印文本报告
    #[arg(long)]
    no_tui: bool,

    /// 持有人扫描模式: auto(先全量后回退) | full(强制全量) | largest(仅Top20)
    #[arg(long, default_value = "auto")]
    holders_mode: String,

    /// 只分析指定钱包（可逗号分隔或多次传入），跳过全量持有人扫描
    #[arg(long, value_delimiter = ',')]
    owners: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let rpc_url = resolve_rpc(args.rpc.as_deref());
    // 公共节点限流很紧，自动降低并发
    let concurrency = if rpc_url.contains("api.mainnet-beta.solana.com") {
        args.concurrency.min(2)
    } else {
        args.concurrency
    };
    eprintln!("RPC: {}", redact(&rpc_url));
    let rpc = Rpc::new(&rpc_url, concurrency);

    let analysis = run_analysis(&rpc, &args).await?;

    if args.no_tui || !std::io::stdout().is_terminal() {
        report::print(&analysis);
    } else {
        tui::run(&analysis)?;
    }
    Ok(())
}

/// RPC 解析顺序: --rpc / SOLANA_RPC_URL → Solana CLI 配置文件 → 公共节点
fn resolve_rpc(cli: Option<&str>) -> String {
    if let Some(u) = cli {
        return u.to_string();
    }
    if let Ok(home) = std::env::var("HOME") {
        let path = format!("{home}/.config/solana/cli/config.yml");
        if let Ok(s) = std::fs::read_to_string(path) {
            for line in s.lines() {
                if let Some(rest) = line.trim().strip_prefix("json_rpc_url:") {
                    let url = rest.trim().trim_matches('"').trim_matches('\'');
                    if url.starts_with("http") {
                        return url.to_string();
                    }
                }
            }
        }
    }
    "https://api.mainnet-beta.solana.com".to_string()
}

/// 日志里隐藏 RPC URL 中的 API key 路径
fn redact(url: &str) -> String {
    match url.split_once(".com/") {
        Some((host, key)) if !key.is_empty() => format!("{host}.com/***"),
        _ => url.to_string(),
    }
}

fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner().with_message(msg.to_string());
    pb.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    pb
}

fn bar(len: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(len).with_message(msg.to_string());
    pb.set_style(
        ProgressStyle::with_template("{msg} [{bar:30}] {pos}/{len}")
            .unwrap()
            .progress_chars("=> "),
    );
    pb
}

async fn run_analysis(rpc: &Rpc, args: &Args) -> Result<Analysis> {
    // 1. 持有人扫描（或 --owners 指定钱包模式）
    let pb = spinner("扫描持有人 (getProgramAccounts)...");
    let (token, all_holders) = if args.owners.is_empty() {
        holders::fetch_holders(rpc, &args.mint, &args.holders_mode).await?
    } else {
        holders::fetch_specified(rpc, &args.mint, &args.owners).await?
    };
    pb.finish_with_message(format!(
        "✓ 持有人扫描完成: {} 个地址{}",
        token.holder_count,
        if token.holders_complete { "" } else { " (非全量)" }
    ));
    let dist = holders::distribution(&token, &all_holders);

    // 2. 选出深度分析目标：跳过池子/交易所等已标记地址
    let targets: Vec<&Holder> = all_holders
        .iter()
        .filter(|h| h.label.is_none())
        .take(args.top)
        .collect();

    // 3. 持有人盈亏
    let pb = bar(targets.len() as u64, "分析持有人盈亏");
    let pnl_results = join_all(targets.iter().map(|h| {
        let pb = pb.clone();
        async move {
            let r = pnl::analyze_holder(
                rpc,
                &h.owner,
                &h.token_accounts,
                &args.mint,
                h.balance,
                args.tx_limit,
            )
            .await;
            pb.inc(1);
            r
        }
    }))
    .await;
    pb.finish_with_message("✓ 盈亏分析完成");
    let mut pnls: Vec<types::HolderPnl> = Vec::new();
    for (h, r) in targets.iter().zip(pnl_results) {
        match r {
            Ok(p) => pnls.push(p),
            Err(e) => eprintln!("⚠ {} 盈亏分析失败: {e}", types::short(&h.owner)),
        }
    }
    // 价格发现：优先从最大的 AMM 池子金库读最新成交，比持有人交易更新鲜
    let mut last_price = None;
    let mut last_price_time = None;
    let pools: Vec<&Holder> = all_holders
        .iter()
        .filter(|h| {
            h.label
                .as_deref()
                .is_some_and(|l| l.contains("池子") || l.contains("AMM") || l.contains("CPMM"))
        })
        .take(2)
        .collect();
    for pool in pools {
        let (px, t) = pnl::pool_price(rpc, &pool.owner, &pool.token_accounts, &args.mint).await;
        if t > last_price_time {
            (last_price, last_price_time) = (px, t);
        }
    }
    if last_price.is_none() {
        (last_price, last_price_time) = pnl::latest_price(&pnls);
    }
    pnl::fill_unrealized(&mut pnls, last_price);

    // 4. 资金溯源
    let pb = bar(targets.len() as u64, "追溯资金来源");
    let flow_results = join_all(targets.iter().map(|h| {
        let pb = pb.clone();
        async move {
            let r = flow::trace_funding(rpc, &h.owner, args.funding_scan).await;
            pb.inc(1);
            r
        }
    }))
    .await;
    pb.finish_with_message("✓ 资金溯源完成");
    let mut flows: Vec<types::HolderFlow> = Vec::new();
    for (h, r) in targets.iter().zip(flow_results) {
        match r {
            Ok(f) => flows.push(f),
            Err(e) => eprintln!("⚠ {} 资金溯源失败: {e}", types::short(&h.owner)),
        }
    }
    flow::annotate_holder_sources(&mut flows, &all_holders);

    // 5. 多跳上游溯源 + SOL/USD 汇率
    let upstream = if args.hops >= 2 {
        let pb = spinner(&format!("追溯上游资金 (hops={})...", args.hops));
        let up = flow::trace_upstream(rpc, &flows, args.hops, args.funding_scan).await;
        pb.finish_with_message(format!("✓ 上游溯源完成: {} 个来源钱包", up.len()));
        up
    } else {
        Default::default()
    };
    let clusters = flow::find_clusters(&flows, &upstream);
    let sol_usd = pnl::sol_usd_price(rpc).await;

    Ok(Analysis {
        token,
        holders: all_holders,
        dist,
        pnl: pnls,
        flows,
        clusters,
        last_price_sol: last_price,
        last_price_time,
        sol_usd,
        upstream,
    })
}
