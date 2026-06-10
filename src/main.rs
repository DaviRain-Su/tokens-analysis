//! tokens-analysis: Solana SPL Token 筹码结构与资金流向分析工具。

mod flow;
mod holders;
mod labels;
mod pnl;
mod report;
mod rpc;
mod trade;
mod tui;
mod types;
mod wallet;
mod watch;

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::future::join_all;
use indicatif::{ProgressBar, ProgressStyle};
use rpc::Rpc;
use std::io::IsTerminal;
use types::{Analysis, Holder};

#[derive(Parser)]
#[command(
    name = "tokens-analysis",
    about = "Solana SPL Token 筹码结构、资金流向分析与聪明钱跟单工具",
    after_help = "示例:\n  tokens-analysis analyze <MINT>\n  tokens-analysis watch --wallets <W1>,<W2> --copy          # paper 跟单\n  tokens-analysis watch --wallets <W1> --copy --live        # 真实下单(谨慎!)\n\n\
                  建议使用支持 getProgramAccounts 的 RPC（如 Triton One, https://docs.triton.one）。"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 分析代币的筹码结构、持有人盈亏与资金流向
    Analyze(Args),
    /// 监控钱包动向，可选自动跟单
    Watch(WatchArgs),
}

#[derive(clap::Args)]
struct WatchArgs {
    /// 要监控的钱包地址（逗号分隔或多次传入）
    #[arg(long, value_delimiter = ',', required = true)]
    wallets: Vec<String>,

    /// RPC 端点。未指定时依次取 env SOLANA_RPC_URL → Solana CLI 配置 → 公共节点
    #[arg(long, env = "SOLANA_RPC_URL")]
    rpc: Option<String>,

    /// 轮询间隔（秒）
    #[arg(long, default_value_t = 5)]
    interval: u64,

    /// 开启跟单（默认 paper 模式，只记录不下单）
    #[arg(long)]
    copy: bool,

    /// 真实下单。务必先用 paper 模式验证！
    #[arg(long)]
    live: bool,

    /// 跳过 live 模式的启动确认
    #[arg(long)]
    yes: bool,

    /// 每次跟买的固定 SOL 金额
    #[arg(long, default_value_t = 0.05)]
    buy_sol: f64,

    /// 每日跟单总额上限 (SOL)
    #[arg(long, default_value_t = 0.5)]
    max_daily_sol: f64,

    /// 滑点上限 (基点, 300 = 3%)
    #[arg(long, default_value_t = 300)]
    slippage_bps: u32,

    /// 目标钱包买入低于该 SOL 金额时不跟（过滤灰尘/试探单）
    #[arg(long, default_value_t = 0.5)]
    min_trigger_sol: f64,

    /// 跟卖时清空全部仓位（默认按目标卖出比例跟随）
    #[arg(long)]
    sell_full: bool,

    /// Jupiter Swap API 基地址
    #[arg(long, default_value = "https://lite-api.jup.ag/swap/v1")]
    jupiter: String,

    /// 签名密钥文件（Solana CLI 格式）
    #[arg(long)]
    keypair: Option<String>,

    /// 跟单审计日志 (JSONL)
    #[arg(long, default_value = "trades.jsonl")]
    log: String,

    /// RPC 并发请求数
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
}

#[derive(clap::Args)]
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
    // 向后兼容：`tokens-analysis <MINT>` 等价于 `tokens-analysis analyze <MINT>`
    let mut argv: Vec<String> = std::env::args().collect();
    if let Some(first) = argv.get(1) {
        if !["analyze", "watch", "help", "-h", "--help", "-V", "--version"]
            .contains(&first.as_str())
        {
            argv.insert(1, "analyze".into());
        }
    }
    let cli = Cli::parse_from(argv);
    match cli.cmd {
        Command::Analyze(args) => cmd_analyze(args).await,
        Command::Watch(args) => cmd_watch(args).await,
    }
}

async fn cmd_analyze(args: Args) -> Result<()> {
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

async fn cmd_watch(args: WatchArgs) -> Result<()> {
    let rpc_url = resolve_rpc(args.rpc.as_deref());
    eprintln!("RPC: {}", redact(&rpc_url));
    let rpc = Rpc::new(&rpc_url, args.concurrency);

    let mut executor = if args.copy {
        let keypair_path = args.keypair.clone().unwrap_or_else(|| {
            format!(
                "{}/.config/solana/id.json",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        let w = wallet::Wallet::load(&keypair_path)?;
        let sol_usd = pnl::sol_usd_price(&rpc).await;
        if let Some(r) = sol_usd {
            println!("SOL/USD: ${r:.2} (稳定币买入信号按此折算触发阈值)");
        }
        let cfg = trade::ExecConfig {
            live: args.live,
            buy_sol: args.buy_sol,
            max_daily_sol: args.max_daily_sol,
            slippage_bps: args.slippage_bps,
            min_trigger_sol: args.min_trigger_sol,
            sell_full: args.sell_full,
            jupiter: args.jupiter.clone(),
            log_path: args.log.clone(),
            sol_usd,
        };
        let exec = trade::Executor::new(cfg, w);
        println!(
            "跟单模式: {}  钱包: {}  单笔 {} SOL  日限 {} SOL  滑点 {}bps  触发阈值 {} SOL",
            if args.live { "\x1b[31mLIVE 真实下单\x1b[0m" } else { "paper" },
            types::short(exec.pubkey()),
            args.buy_sol,
            args.max_daily_sol,
            args.slippage_bps,
            args.min_trigger_sol,
        );
        if args.live && !args.yes {
            print!("⚠ LIVE 模式将用真实资金下单。输入 yes 继续: ");
            use std::io::Write;
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if line.trim() != "yes" {
                println!("已取消。");
                return Ok(());
            }
        }
        Some(exec)
    } else {
        None
    };

    let mut watcher = watch::Watcher::new(&rpc, &args.wallets).await?;
    watcher.run(&rpc, args.interval, executor.as_mut()).await
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
