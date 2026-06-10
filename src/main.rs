//! tokens-analysis: Solana SPL Token 筹码结构与资金流向分析工具。

mod flow;
mod holders;
mod labels;
mod meta;
mod notify;
mod pnl;
mod report;
mod rpc;
mod safety;
mod snapshot;
mod trade;
mod tui;
mod types;
mod wallet;
mod watch;
mod ws;

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
    #[arg(long, value_delimiter = ',')]
    wallets: Vec<String>,

    /// 从文件读取监控钱包（每行一个地址，或 analyze --export-smart-money 的 JSONL）
    #[arg(long)]
    wallets_file: Option<String>,

    /// 跳过买前安全检查（可增发/冻结/转账税的代币也跟买，不建议）
    #[arg(long)]
    allow_risky: bool,

    /// RPC 端点。未指定时依次取 env SOLANA_RPC_URL → Solana CLI 配置 → 公共节点
    #[arg(long, env = "SOLANA_RPC_URL")]
    rpc: Option<String>,

    /// 轮询间隔（秒，仅轮询模式）
    #[arg(long, default_value_t = 5)]
    interval: u64,

    /// 禁用 WebSocket 实时推送，强制轮询
    #[arg(long)]
    no_ws: bool,

    /// WebSocket 端点（默认由 RPC URL 推导: https→wss）
    #[arg(long)]
    ws_url: Option<String>,

    /// 止盈倍数（如 2.0 = 现值达成本 2 倍清仓），0 = 关闭
    #[arg(long, default_value_t = 0.0)]
    take_profit: f64,

    /// 止损倍数（如 0.5 = 现值跌到成本一半清仓），0 = 关闭
    #[arg(long, default_value_t = 0.0)]
    stop_loss: f64,

    /// 止盈止损巡检间隔（秒）
    #[arg(long, default_value_t = 20)]
    price_check_interval: u64,

    /// 仓位持久化文件（默认 paper 模式 positions-paper.json，live 模式 positions.json）
    #[arg(long)]
    positions_file: Option<String>,

    /// 买卖执行后推送通知: desktop (macOS) | telegram (env TELEGRAM_BOT_TOKEN/CHAT_ID)
    #[arg(long)]
    notify: Option<String>,

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

    /// 把聪明钱钱包导出到 JSONL 文件（可直接喂给 watch --wallets-file）
    #[arg(long)]
    export_smart_money: Option<String>,

    /// 导出聪明钱的最低评分
    #[arg(long, default_value_t = 40.0)]
    smart_min_score: f64,

    /// 分析后保存持有人快照到 snapshots/<mint>/
    #[arg(long)]
    snapshot: bool,

    /// 与历史快照对比筹码迁移（不带值 = 最近一次快照，或指定文件路径）
    #[arg(long, num_args = 0..=1, default_missing_value = "latest")]
    diff: Option<String>,
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

    if let Some(path) = &args.export_smart_money {
        export_smart_money(&analysis, path, args.smart_min_score)?;
    }

    if args.no_tui || !std::io::stdout().is_terminal() {
        report::print(&analysis);
    } else {
        tui::run(&analysis)?;
    }
    Ok(())
}

/// 按评分导出聪明钱钱包 (JSONL)，可直接喂给 watch --wallets-file。
fn export_smart_money(a: &Analysis, path: &str, min_score: f64) -> Result<()> {
    let mut scored: Vec<(pnl::SmartMetrics, &types::HolderPnl)> = a
        .pnl
        .iter()
        .filter_map(|p| pnl::smart_metrics(p, a.sol_usd).map(|m| (m, p)))
        .filter(|(m, _)| m.score >= min_score)
        .collect();
    scored.sort_by(|x, y| y.0.score.total_cmp(&x.0.score));
    let mut out = String::new();
    for (m, p) in &scored {
        out.push_str(
            &serde_json::json!({
                "owner": p.owner,
                "score": (m.score * 10.0).round() / 10.0,
                "invested_sol": m.invested_sol,
                "total_pnl_sol": m.total_pnl_sol,
                "roi": m.roi,
                "realized_sol": p.realized_sol,
                "unrealized_sol": p.unrealized_sol,
                "mint": a.token.mint,
            })
            .to_string(),
        );
        out.push('\n');
    }
    std::fs::write(path, &out)?;
    eprintln!(
        "✓ 已导出 {} 个聪明钱钱包 (评分 ≥ {min_score}) 到 {path}",
        scored.len()
    );
    if scored.is_empty() {
        eprintln!("  提示: 没有达标钱包。Top 大户多为转账型机构钱包，可加大 --top 覆盖更多真实交易者。");
    }
    Ok(())
}

/// 解析 --wallets-file：支持纯地址行或 export-smart-money 的 JSONL。
fn load_wallets_file(path: &str) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let addr = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v["owner"].as_str().map(String::from))
            .unwrap_or_else(|| line.to_string());
        if !out.contains(&addr) {
            out.push(addr);
        }
    }
    Ok(out)
}

async fn cmd_watch(mut args: WatchArgs) -> Result<()> {
    if let Some(path) = &args.wallets_file {
        for w in load_wallets_file(path)? {
            if !args.wallets.contains(&w) {
                args.wallets.push(w);
            }
        }
    }
    if args.wallets.is_empty() {
        anyhow::bail!("没有要监控的钱包：请用 --wallets 或 --wallets-file 指定");
    }
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
            allow_risky: args.allow_risky,
            take_profit: args.take_profit,
            stop_loss: args.stop_loss,
            positions_path: args.positions_file.clone().unwrap_or_else(|| {
                if args.live { "positions.json" } else { "positions-paper.json" }.into()
            }),
            notifier: args
                .notify
                .as_deref()
                .map(notify::Notifier::from_kind)
                .transpose()?,
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

    if args.take_profit > 0.0 || args.stop_loss > 0.0 {
        println!(
            "止盈止损: TP={}x SL={}x (每 {}s 巡检)",
            args.take_profit, args.stop_loss, args.price_check_interval
        );
    }
    if let Some(exec) = &executor {
        for (mint, raw, cost) in exec.positions_summary() {
            println!("  持仓: {} raw={raw} 成本={cost:.4} SOL", types::short(&mint));
        }
    }

    let mut watcher = watch::Watcher::new(&rpc, &args.wallets).await?;
    if args.no_ws {
        return watcher
            .run_polling(&rpc, args.interval, executor.as_mut())
            .await;
    }
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| ws::derive_ws_url(&rpc_url));
    watcher
        .run_ws(&rpc, ws_url, args.price_check_interval, executor.as_mut())
        .await
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
    let safety = safety::check_mint(rpc, &args.mint).await.ok();
    let symbol = meta::fetch_meta(rpc, &args.mint).await.map(|m| m.symbol);
    let transfer_links = build_transfer_links(&pnls);

    // 快照: 先 diff 旧的，再保存新的
    let snapshot_diff = match &args.diff {
        Some(path) => match snapshot::load(&args.mint, path) {
            Ok(old) => Some(snapshot::diff(&old, &all_holders)),
            Err(e) => {
                eprintln!("⚠ 快照对比失败: {e}");
                None
            }
        },
        None => None,
    };
    if args.snapshot {
        match snapshot::save(&args.mint, &all_holders, chrono::Utc::now().timestamp()) {
            Ok(p) => eprintln!("✓ 快照已保存: {p}"),
            Err(e) => eprintln!("⚠ 快照保存失败: {e}"),
        }
    }

    let mut token = token;
    token.symbol = symbol;
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
        safety,
        upstream,
        transfer_links,
        snapshot_diff,
    })
}

/// 聚合所有已分析持有人的转账事件为互转边 (from → to)。
fn build_transfer_links(pnls: &[types::HolderPnl]) -> Vec<types::TransferLink> {
    use std::collections::{HashMap, HashSet};
    let mut edges: HashMap<(String, String), types::TransferLink> = HashMap::new();
    // 双方都被分析时同一笔转账会出现两次（一边转出一边转入），按签名去重
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    for p in pnls {
        for e in &p.events {
            let Some(cp) = &e.counterparty else { continue };
            let (from, to) = match e.side {
                types::Side::TransferIn => (cp.clone(), p.owner.clone()),
                types::Side::TransferOut => (p.owner.clone(), cp.clone()),
                _ => continue,
            };
            if !seen.insert((from.clone(), to.clone(), e.signature.clone())) {
                continue;
            }
            let link = edges
                .entry((from.clone(), to.clone()))
                .or_insert_with(|| types::TransferLink {
                    from,
                    to,
                    tokens: 0.0,
                    count: 0,
                    last_time: None,
                });
            link.tokens += e.token_amount;
            link.count += 1;
            if e.time > link.last_time {
                link.last_time = e.time;
            }
        }
    }
    let mut links: Vec<types::TransferLink> = edges.into_values().collect();
    links.sort_by(|a, b| b.tokens.total_cmp(&a.tokens));
    links.truncate(50);
    links
}
