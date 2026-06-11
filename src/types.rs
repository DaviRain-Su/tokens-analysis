//! 共享数据模型。

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenInfo {
    pub mint: String,
    /// Metaplex / Token-2022 元数据里的 symbol
    pub symbol: Option<String>,
    pub program: String,
    pub decimals: u8,
    pub supply: f64,
    /// 余额 > 0 的持有人（钱包）数量
    pub holder_count: usize,
    /// false 表示 getProgramAccounts 不可用，只拿到了 Top20 代币账户
    pub holders_complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Holder {
    pub owner: String,
    pub token_accounts: Vec<String>,
    pub balance: f64,
    /// 占总供应量百分比 (0-100)
    pub pct: f64,
    /// 已知标签（交易所 / AMM 池子 / 程序账户）
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
    TransferIn,
    TransferOut,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TradeEvent {
    /// 暂未在界面展示，保留用于后续交易明细视图
    #[allow(dead_code)]
    pub signature: String,
    pub time: Option<i64>,
    pub side: Side,
    /// 代币数量（恒为正）
    pub token_amount: f64,
    /// SOL 数量（恒为正，转账类为 0）
    pub sol_amount: f64,
    /// 稳定币 (USDC/USDT) 数量（恒为正）
    pub usd_amount: f64,
    /// SOL 计价成交价
    pub price_sol: Option<f64>,
    /// 转账对手方钱包（仅转入/转出时有值；买卖的对手是池子，不记录）
    pub counterparty: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HolderPnl {
    pub owner: String,
    pub events: Vec<TradeEvent>,
    /// 当前仓位（按账本推算，应 ≈ 链上余额）
    pub position: f64,
    /// 当前仓位剩余 SOL 成本
    pub cost_sol: f64,
    pub bought_tokens: f64,
    pub sold_tokens: f64,
    pub transfer_in: f64,
    pub transfer_out: f64,
    pub sol_spent: f64,
    pub sol_received: f64,
    pub usd_spent: f64,
    pub usd_received: f64,
    /// SOL 计价的已实现盈亏
    pub realized_sol: f64,
    /// SOL 计价的浮动盈亏（需要最新价格，分析完统一回填）
    pub unrealized_sol: Option<f64>,
    /// 当前仓位的平均成本 (SOL/token)
    pub avg_cost_sol: Option<f64>,
    /// 历史被截断 / 存在转入或稳定币买入等无法定价的代币
    pub has_unknown_cost: bool,
    /// 交易历史超过扫描上限，账本不完整
    pub partial_history: bool,
    pub first_time: Option<i64>,
    pub last_time: Option<i64>,
}

/// 监控模式下解析出的单笔钱包动向
#[derive(Clone, Debug)]
pub struct WatchEvent {
    pub mint: String,
    /// 代币 symbol（监控循环填充，可能未知）
    pub symbol: Option<String>,
    pub event: TradeEvent,
    /// 该钱包此 mint 的交易前/后余额（按比例跟卖时需要）
    pub pre_balance: f64,
    pub post_balance: f64,
}

impl WatchEvent {
    pub fn token_disp(&self) -> String {
        match &self.symbol {
            Some(s) => format!("{s}({})", short(&self.mint)),
            None => short(&self.mint),
        }
    }
}

/// Top 持有人之间的代币互转聚合边
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferLink {
    pub from: String,
    pub to: String,
    pub tokens: f64,
    pub count: usize,
    pub last_time: Option<i64>,
}

/// 与历史快照的筹码对比
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub base_time: i64,
    /// (owner, 旧余额, 新余额)，按变化量绝对值降序
    pub changes: Vec<(String, f64, f64)>,
    pub new_holders: usize,
    pub exited_holders: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundingSource {
    pub source: String,
    pub label: Option<String>,
    pub total_sol: f64,
    pub count: usize,
    pub first_time: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HolderFlow {
    pub owner: String,
    pub sources: Vec<FundingSource>,
    pub scanned_txs: usize,
    /// true 表示扫到了钱包的最早历史（资金溯源可信度高）
    pub reached_genesis: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cluster {
    pub source: String,
    pub label: Option<String>,
    pub holders: Vec<String>,
    pub total_sol: f64,
    /// 各持有人首次入金时间的跨度（秒）。极小说明是同一批集中注资（钱包农场特征）
    pub time_span_secs: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Distribution {
    pub top1_pct: f64,
    pub top10_pct: f64,
    pub top20_pct: f64,
    pub top100_pct: f64,
    /// Herfindahl-Hirschman 集中度指数 (0-10000)
    pub hhi: f64,
    /// (名称, 持有人数量, 供应占比%)
    pub buckets: Vec<(String, usize, f64)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Analysis {
    pub token: TokenInfo,
    /// 按余额降序的全部持有人（或回退模式下的 Top20）
    pub holders: Vec<Holder>,
    pub dist: Distribution,
    /// 深度分析的持有人盈亏（与 analyzed_owners 顺序一致）
    pub pnl: Vec<HolderPnl>,
    pub flows: Vec<HolderFlow>,
    pub clusters: Vec<Cluster>,
    /// 全部已解析交易里最近一笔 SOL 计价成交价
    pub last_price_sol: Option<f64>,
    pub last_price_time: Option<i64>,
    /// SOL/USD 汇率（从 Raydium SOL/USDC 池最近成交推导）
    pub sol_usd: Option<f64>,
    /// 代币机制安全检查结果
    pub safety: Option<crate::safety::SafetyReport>,
    /// Top 持有人之间的代币互转
    pub transfer_links: Vec<TransferLink>,
    /// 与上次快照的筹码迁移对比
    pub snapshot_diff: Option<SnapshotDiff>,
    /// 多跳资金溯源：来源钱包 → 它自己的上游入金（--hops >= 2 时填充）
    pub upstream: std::collections::HashMap<String, Vec<FundingSource>>,
}

pub fn fmt_time(t: Option<i64>) -> String {
    t.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

/// 1234567.0 -> "1.23M"
pub fn human(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1e9 {
        format!("{:.2}B", n / 1e9)
    } else if abs >= 1e6 {
        format!("{:.2}M", n / 1e6)
    } else if abs >= 1e3 {
        format!("{:.2}K", n / 1e3)
    } else if abs >= 1.0 {
        format!("{n:.2}")
    } else if abs == 0.0 {
        "0".into()
    } else {
        format!("{n:.6}")
    }
}

pub fn short(addr: &str) -> String {
    if addr.len() <= 12 {
        addr.to_string()
    } else {
        format!("{}..{}", &addr[..5], &addr[addr.len() - 5..])
    }
}
