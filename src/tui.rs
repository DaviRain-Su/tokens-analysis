//! ratatui 交互界面：概览 / 筹码结构 / 持有人盈亏 / 资金流向 / 关联集群 五个标签页。

use crate::report::status_text;
use crate::types::{Analysis, HolderPnl, fmt_time, human, short};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, TableState, Tabs};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
use std::time::Duration;

const TABS: [&str; 5] = ["概览", "筹码结构", "持有人盈亏", "资金流向", "关联集群"];

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Balance,
    Unrealized,
    Realized,
    Score,
}

pub struct App<'a> {
    a: &'a Analysis,
    tab: usize,
    tables: [TableState; 5],
    sort: SortBy,
    pnl_by_owner: HashMap<&'a str, &'a HolderPnl>,
    flow_rows: Vec<FlowRow<'a>>,
    /// 持有人盈亏页按 Enter 打开的交易明细 (owner, 滚动状态)
    detail: Option<(String, TableState)>,
}

struct FlowRow<'a> {
    owner: &'a str,
    rank: usize,
    source: Option<&'a crate::types::FundingSource>,
    reached_genesis: bool,
    /// 0 = 直接入金, 1 = 上游(第二跳)
    depth: usize,
}

pub fn run(a: &Analysis) -> Result<()> {
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, a);
    ratatui::restore();
    res
}

fn event_loop(
    terminal: &mut Terminal<impl ratatui::backend::Backend>,
    a: &Analysis,
) -> Result<()> {
    let pnl_by_owner: HashMap<&str, &HolderPnl> =
        a.pnl.iter().map(|p| (p.owner.as_str(), p)).collect();
    let rank_of: HashMap<&str, usize> = a
        .holders
        .iter()
        .enumerate()
        .map(|(i, h)| (h.owner.as_str(), i + 1))
        .collect();
    let mut flow_rows = Vec::new();
    for f in &a.flows {
        let rank = rank_of.get(f.owner.as_str()).copied().unwrap_or(0);
        if f.sources.is_empty() {
            flow_rows.push(FlowRow {
                owner: &f.owner,
                rank,
                source: None,
                reached_genesis: f.reached_genesis,
                depth: 0,
            });
        }
        for s in f.sources.iter().take(5) {
            flow_rows.push(FlowRow {
                owner: &f.owner,
                rank,
                source: Some(s),
                reached_genesis: f.reached_genesis,
                depth: 0,
            });
            for u in a.upstream.get(&s.source).into_iter().flatten().take(3) {
                flow_rows.push(FlowRow {
                    owner: &f.owner,
                    rank,
                    source: Some(u),
                    reached_genesis: f.reached_genesis,
                    depth: 1,
                });
            }
        }
    }

    let mut app = App {
        a,
        tab: 0,
        tables: Default::default(),
        sort: SortBy::Balance,
        pnl_by_owner,
        flow_rows,
        detail: None,
    };
    for t in &mut app.tables {
        t.select(Some(0));
    }

    loop {
        terminal.draw(|f| draw(f, &mut app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // 交易明细弹层打开时按键只作用于明细表
            if app.detail.is_some() {
                let len = app
                    .detail
                    .as_ref()
                    .and_then(|(o, _)| app.pnl_by_owner.get(o.as_str()))
                    .map(|p| p.events.len())
                    .unwrap_or(0);
                let (_, state) = app.detail.as_mut().unwrap();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Backspace => {
                        app.detail = None;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some((i + 1).min(len.saturating_sub(1))));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some(i.saturating_sub(1)));
                    }
                    KeyCode::PageDown => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some((i + 15).min(len.saturating_sub(1))));
                    }
                    KeyCode::PageUp => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some(i.saturating_sub(15)));
                    }
                    _ => {}
                }
                continue;
            }
            let len = app.row_count();
            let state = &mut app.tables[app.tab];
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Tab | KeyCode::Right => app.tab = (app.tab + 1) % TABS.len(),
                KeyCode::BackTab | KeyCode::Left => {
                    app.tab = (app.tab + TABS.len() - 1) % TABS.len()
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = state.selected().unwrap_or(0);
                    state.select(Some((i + 1).min(len.saturating_sub(1))));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = state.selected().unwrap_or(0);
                    state.select(Some(i.saturating_sub(1)));
                }
                KeyCode::PageDown => {
                    let i = state.selected().unwrap_or(0);
                    state.select(Some((i + 15).min(len.saturating_sub(1))));
                }
                KeyCode::PageUp => {
                    let i = state.selected().unwrap_or(0);
                    state.select(Some(i.saturating_sub(15)));
                }
                KeyCode::Char('s') if app.tab == 2 => {
                    app.sort = match app.sort {
                        SortBy::Balance => SortBy::Unrealized,
                        SortBy::Unrealized => SortBy::Realized,
                        SortBy::Realized => SortBy::Score,
                        SortBy::Score => SortBy::Balance,
                    };
                }
                KeyCode::Enter if app.tab == 2 => {
                    let i = app.tables[2].selected().unwrap_or(0);
                    if let Some(h) = app.sorted_holders().get(i) {
                        if app.pnl_by_owner.contains_key(h.owner.as_str()) {
                            let mut st = TableState::default();
                            st.select(Some(0));
                            app.detail = Some((h.owner.clone(), st));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'a> App<'a> {
    fn row_count(&self) -> usize {
        match self.tab {
            2 => self.a.holders.len().min(100),
            3 => self.flow_rows.len(),
            4 => self.a.clusters.len(),
            _ => 0,
        }
    }

    /// 持有人盈亏页当前排序下的行顺序（渲染与 Enter 选中共用，保证一致）
    fn sorted_holders(&self) -> Vec<&'a crate::types::Holder> {
        let mut holders: Vec<&crate::types::Holder> = self.a.holders.iter().take(100).collect();
        if self.sort != SortBy::Balance {
            holders.sort_by(|x, y| {
                let key = |h: &crate::types::Holder| {
                    self.pnl_by_owner
                        .get(h.owner.as_str())
                        .map(|p| match self.sort {
                            SortBy::Unrealized => p.unrealized_sol.unwrap_or(f64::NEG_INFINITY),
                            SortBy::Score => crate::pnl::smart_score(p, self.a.sol_usd)
                                .unwrap_or(f64::NEG_INFINITY),
                            _ => p.realized_sol,
                        })
                        .unwrap_or(f64::NEG_INFINITY)
                };
                key(y).total_cmp(&key(x))
            });
        }
        holders
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    let t = &app.a.token;
    let price = app
        .a
        .last_price_sol
        .map(|p| {
            let usd = app
                .a
                .sol_usd
                .map(|r| format!(" (${:.8})", p * r))
                .unwrap_or_default();
            format!("{p:.10} SOL{usd}")
        })
        .unwrap_or_else(|| "-".into());
    let header = Line::from(vec![
        Span::styled(" SPL Token 分析 ", Style::new().bold().fg(Color::Cyan)),
        Span::raw(format!(
            "{}{}  供应 {}  持有人 {}{}  价格 {}",
            t.symbol
                .as_deref()
                .map(|s| format!("{s} "))
                .unwrap_or_default(),
            short(&t.mint),
            human(t.supply),
            t.holder_count,
            if t.holders_complete { "" } else { "(Top20)" },
            price,
        )),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    let tabs = Tabs::new(TABS.iter().map(|s| Line::from(*s)))
        .select(app.tab)
        .highlight_style(Style::new().bold().fg(Color::Yellow).underlined())
        .block(Block::new().borders(Borders::BOTTOM));
    f.render_widget(tabs, chunks[1]);

    if app.detail.is_some() {
        draw_detail(f, chunks[2], app);
    } else {
        match app.tab {
            0 => draw_overview(f, chunks[2], app),
            1 => draw_chips(f, chunks[2], app),
            2 => draw_holders(f, chunks[2], app),
            3 => draw_flow(f, chunks[2], app),
            _ => draw_clusters(f, chunks[2], app),
        }
    }

    let help = if app.detail.is_some() {
        " Esc 返回 | ↑↓ 滚动"
    } else {
        match app.tab {
            1 => " q 退出 | ←→/Tab 切换标签 | ↑↓ 滚动 | s 切换排序 | Enter 交易明细",
            _ => " q 退出 | ←→/Tab 切换标签 | ↑↓ 滚动",
        }
    };
    f.render_widget(
        Paragraph::new(help).style(Style::new().fg(Color::DarkGray)),
        chunks[3],
    );
}

fn draw_overview(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let t = &app.a.token;
    let d = &app.a.dist;
    let mut lines = vec![
        Line::from(format!("Mint:      {}", t.mint)),
        Line::from(format!("Program:   {}", short(&t.program))),
        Line::from(format!("总供应量:  {}", human(t.supply))),
        Line::from(format!("精度:      {}", t.decimals)),
        Line::from(format!(
            "持有人:    {}{}",
            t.holder_count,
            if t.holders_complete { "" } else { " (仅Top20)" }
        )),
        Line::from(""),
        Line::from(Span::styled("集中度指标", Style::new().bold())),
        Line::from(format!("HHI:       {:.0} / 10000", d.hhi)),
        Line::from(format!("已深度分析: {} 个地址", app.a.pnl.len())),
        Line::from(""),
    ];
    if let Some(px) = app.a.last_price_sol {
        lines.insert(
            5,
            Line::from(format!(
                "最新成交:  {:.10} SOL ({})",
                px,
                fmt_time(app.a.last_price_time)
            )),
        );
    }
    if let Some(r) = app.a.sol_usd {
        lines.push(Line::from(format!("SOL/USD:   ${r:.2}")));
    }
    if let Some(s) = &app.a.safety {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("安全检查", Style::new().bold())));
        if s.is_safe() {
            lines.push(Line::from(Span::styled(
                "✓ 通过 (无机制风险)",
                Style::new().fg(Color::Green),
            )));
        } else {
            for risk in &s.risks {
                lines.push(Line::from(Span::styled(
                    format!("✗ {risk}"),
                    Style::new().fg(Color::Red),
                )));
            }
        }
    }
    lines.push(Line::from(Span::styled("筹码分层", Style::new().bold())));
    for (name, count, pct) in &d.buckets {
        lines.push(Line::from(format!(
            "{name:<14} {count:>7} 地址  {pct:>6.2}%"
        )));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" 代币信息 ")),
        cols[0],
    );

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(cols[1]);
    for (i, (name, pct)) in [
        ("Top 1", d.top1_pct),
        ("Top 10", d.top10_pct),
        ("Top 20", d.top20_pct),
        ("Top 100", d.top100_pct),
    ]
    .iter()
    .enumerate()
    {
        let color = if *pct > 50.0 {
            Color::Red
        } else if *pct > 25.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        f.render_widget(
            Gauge::default()
                .block(Block::bordered().title(format!(" {name} 占比 ")))
                .gauge_style(Style::new().fg(color))
                .ratio((pct / 100.0).clamp(0.0, 1.0))
                .label(format!("{pct:.2}%")),
            rows[i],
        );
    }
}

fn pnl_span(v: f64, fmt: String) -> Span<'static> {
    if v > 0.0 {
        Span::styled(fmt, Style::new().fg(Color::Green))
    } else if v < 0.0 {
        Span::styled(fmt, Style::new().fg(Color::Red))
    } else {
        Span::raw(fmt)
    }
}

/// 筹码结构页：左=占比瓜分条+盈亏分布，右=筹码成本分布（筹码峰）
fn draw_chips(f: &mut Frame, area: Rect, app: &App) {
    use crate::chart;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(cols[0]);

    // ── 占比瓜分条
    let shares = chart::holder_shares(&app.a.holders, 8);
    let mut lines = vec![Line::from(chart::shares_bar(&shares, 46))];
    lines.push(Line::from(Span::styled(
        "巨鲸█ 大户▓ 中户▒ 散户░",
        Style::new().fg(Color::DarkGray),
    )));
    for s in shares.top.iter().take(8) {
        let color = match s.tier {
            0 => Color::Red,
            1 => Color::Yellow,
            2 => Color::Cyan,
            _ => Color::DarkGray,
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", chart::tier_char(s.tier)), Style::new().fg(color)),
            Span::raw(format!("#{:<2} {} {:>6.2}%  ", s.rank, short(&s.owner), s.pct)),
            Span::styled(
                s.label.clone().unwrap_or_default(),
                Style::new().fg(Color::Yellow),
            ),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered().title(format!(" 流通筹码瓜分 (已扫描 {:.1}%) ", shares.covered_pct)),
        ),
        left[0],
    );

    // ── 盈亏分布
    let mut pnl_lines = Vec::new();
    if let Some(d) = chart::pnl_distribution(&app.a.pnl) {
        for (i, r) in d.rows.iter().take(12).enumerate() {
            let color = if r.unrealized_sol >= 0.0 { Color::Green } else { Color::Red };
            let sign = if r.unrealized_sol >= 0.0 { "▲" } else { "▼" };
            pnl_lines.push(Line::from(vec![
                Span::raw(format!("#{:<2}{} ", i + 1, short(&r.owner))),
                Span::styled(chart::bar(r.position / d.max_position, 10), Style::new().fg(color)),
                Span::styled(
                    format!(" {sign}{:.2}◎", r.unrealized_sol),
                    Style::new().fg(color),
                ),
            ]));
        }
        let total = d.profit_position + d.loss_position;
        pnl_lines.push(Line::from(Span::styled(
            format!(
                "获利 {} 人 {} | 套牢 {} 人 {}",
                d.profit_holders,
                chart::amount_pct(d.profit_position, total),
                d.loss_holders,
                chart::amount_pct(d.loss_position, total),
            ),
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        pnl_lines.push(Line::from("  (Top 持有人多为转账型钱包，无浮动盈亏数据)"));
    }
    f.render_widget(
        Paragraph::new(pnl_lines).block(Block::bordered().title(" 持有人浮动盈亏 (柱长=持仓量) ")),
        left[1],
    );

    // ── 筹码成本分布（筹码峰）
    let unit = app.a.token.symbol.as_deref().unwrap_or("token");
    let mut peak_lines = Vec::new();
    if let Some(h) = chart::cost_distribution(&app.a.pnl, app.a.last_price_sol, 12) {
        let price_line = |px: f64| {
            Line::from(Span::styled(
                format!("─ 现价 {}◎ ─────────", chart::fmt_price(px)),
                Style::new().fg(Color::Magenta).bold(),
            ))
        };
        if let Some(px) = h.price {
            if h.buckets.iter().filter(|b| b.amount > 0.0).all(|b| b.price < px) {
                peak_lines.push(price_line(px));
            }
        }
        let mut prev: Option<f64> = None;
        for b in &h.buckets {
            if b.amount <= 0.0 {
                continue;
            }
            if let Some(px) = h.price {
                if prev.is_some_and(|p| p > px) && b.price <= px {
                    peak_lines.push(price_line(px));
                }
            }
            let color = if b.underwater { Color::Red } else { Color::Green };
            peak_lines.push(Line::from(vec![
                Span::raw(format!("{}◎ ", chart::fmt_price(b.price))),
                Span::styled(chart::bar(b.amount / h.max_amount, 16), Style::new().fg(color)),
            ]));
            prev = Some(b.price);
        }
        if let Some(px) = h.price {
            if h.buckets.iter().filter(|b| b.amount > 0.0).all(|b| b.price > px) {
                peak_lines.push(price_line(px));
            }
        }
        let total = h.profit_amount + h.underwater_amount + h.unknown_amount;
        peak_lines.push(Line::from(""));
        peak_lines.push(Line::from(vec![
            Span::styled(
                format!("获利 {} ", chart::amount_pct(h.profit_amount, total)),
                Style::new().fg(Color::Green),
            ),
            Span::styled(
                format!("套牢 {} ", chart::amount_pct(h.underwater_amount, total)),
                Style::new().fg(Color::Red),
            ),
        ]));
        peak_lines.push(Line::from(Span::styled(
            format!("成本未知 {}", chart::amount_pct(h.unknown_amount, total)),
            Style::new().fg(Color::DarkGray),
        )));
        if let Some(peak) = h.peak_price {
            peak_lines.push(Line::from(format!("主力成本区 {}◎", chart::fmt_price(peak))));
        }
    } else {
        peak_lines.push(Line::from("  (无成本数据：Top 持有人多为转账型/未扫到买入)"));
    }
    f.render_widget(
        Paragraph::new(peak_lines).block(
            Block::bordered().title(format!(" 筹码成本分布 ({unit}, 柱长=该价位筹码量) ")),
        ),
        cols[1],
    );
}

fn draw_holders(f: &mut Frame, area: Rect, app: &mut App) {
    let holders = app.sorted_holders();

    let rows = holders.iter().enumerate().map(|(i, h)| {
        let p = app.pnl_by_owner.get(h.owner.as_str());
        let cells: Vec<Cell> = vec![
            Cell::from(format!("{}", i + 1)),
            Cell::from(short(&h.owner)),
            Cell::from(human(h.balance)),
            Cell::from(format!("{:.2}", h.pct)),
            Cell::from(p.map(|p| human(p.bought_tokens)).unwrap_or_else(|| "-".into())),
            Cell::from(p.map(|p| human(p.sold_tokens)).unwrap_or_else(|| "-".into())),
            Cell::from(p.map(|p| human(p.transfer_in)).unwrap_or_else(|| "-".into())),
            Cell::from(p.map(|p| human(p.transfer_out)).unwrap_or_else(|| "-".into())),
            Cell::from(
                p.and_then(|p| p.avg_cost_sol)
                    .map(|v| format!("{v:.9}"))
                    .unwrap_or_else(|| "-".into()),
            ),
            p.map(|p| Cell::from(pnl_span(p.realized_sol, format!("{:+.3}", p.realized_sol))))
                .unwrap_or_else(|| Cell::from("-")),
            p.and_then(|p| p.unrealized_sol)
                .map(|v| Cell::from(pnl_span(v, format!("{v:+.3}"))))
                .unwrap_or_else(|| Cell::from("-")),
            p.and_then(|p| crate::pnl::smart_score(p, app.a.sol_usd))
                .map(|s| {
                    let style = if s >= 70.0 {
                        Style::new().fg(Color::Green).bold()
                    } else if s >= 40.0 {
                        Style::new().fg(Color::Yellow)
                    } else {
                        Style::new()
                    };
                    Cell::from(Span::styled(format!("{s:.0}"), style))
                })
                .unwrap_or_else(|| Cell::from("-")),
            Cell::from(
                p.map(|p| status_text(p))
                    .unwrap_or_else(|| h.label.clone().unwrap_or_default()),
            ),
        ];
        Row::new(cells)
    });

    let sort_name = match app.sort {
        SortBy::Balance => "余额",
        SortBy::Unrealized => "浮动盈亏",
        SortBy::Realized => "已实现盈亏",
        SortBy::Score => "聪明钱评分",
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(13),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(13),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Min(8),
        ],
    )
    .header(
        Row::new([
            "#", "地址", "余额", "占比%", "买入量", "卖出量", "转入量", "转出量", "均价SOL",
            "已实现", "浮动", "评分", "状态",
        ])
        .style(Style::new().bold().fg(Color::Cyan)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title(format!(
        " Top 持有人 (SOL 计价盈亏, 排序: {sort_name}, ~ 表示历史不完整) "
    )));
    f.render_stateful_widget(table, area, &mut app.tables[2]);
}

/// 单个持有人的交易明细（持有人盈亏页 Enter 打开）
fn draw_detail(f: &mut Frame, area: Rect, app: &mut App) {
    let Some((owner, _)) = &app.detail else { return };
    let owner = owner.clone();
    let Some(p) = app.pnl_by_owner.get(owner.as_str()) else {
        return;
    };
    let sol_usd = app.a.sol_usd;
    let rows = p.events.iter().rev().map(|e| {
        let (side, color) = match e.side {
            crate::types::Side::Buy => ("买入", Color::Green),
            crate::types::Side::Sell => ("卖出", Color::Red),
            crate::types::Side::TransferIn => ("转入", Color::Cyan),
            crate::types::Side::TransferOut => ("转出", Color::Yellow),
        };
        let value = if e.sol_amount > 0.0 {
            format!("{:.4} SOL", e.sol_amount)
        } else if e.usd_amount > 0.0 {
            format!("${:.2}", e.usd_amount)
        } else {
            "-".into()
        };
        let price = e
            .price_sol
            .map(|px| {
                let usd = sol_usd
                    .map(|r| format!(" (${:.8})", px * r))
                    .unwrap_or_default();
                format!("{px:.10}{usd}")
            })
            .unwrap_or_else(|| "-".into());
        Row::new(vec![
            Cell::from(fmt_time(e.time)),
            Cell::from(Span::styled(side, Style::new().fg(color).bold())),
            Cell::from(human(e.token_amount)),
            Cell::from(value),
            Cell::from(price),
            Cell::from(short(&e.signature)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(17),
            Constraint::Length(6),
            Constraint::Length(11),
            Constraint::Length(14),
            Constraint::Length(26),
            Constraint::Min(12),
        ],
    )
    .header(
        Row::new(["时间", "方向", "数量", "对价", "价格(SOL)", "签名"])
            .style(Style::new().bold().fg(Color::Cyan)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title(format!(
        " {} 交易明细 ({} 笔, 新→旧{}) ",
        short(&owner),
        p.events.len(),
        if p.partial_history { ", 历史已截断" } else { "" }
    )));
    let state = &mut app.detail.as_mut().unwrap().1;
    f.render_stateful_widget(table, area, state);
}

fn draw_flow(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.flow_rows.iter().map(|r| {
        match r.source {
            Some(s) => Row::new(vec![
                Cell::from(if r.depth == 0 {
                    format!("#{}", r.rank)
                } else {
                    String::new()
                }),
                Cell::from(if r.depth == 0 {
                    short(r.owner)
                } else {
                    String::new()
                }),
                Cell::from(if r.depth == 0 {
                    format!("← {}", short(&s.source))
                } else {
                    format!("  ↖ {}", short(&s.source))
                }),
                Cell::from(match &s.label {
                    Some(l) => Span::styled(l.clone(), Style::new().fg(Color::Yellow)),
                    None => Span::raw(""),
                }),
                Cell::from(format!("{:.4}", s.total_sol)),
                Cell::from(format!("×{}", s.count)),
                Cell::from(fmt_time(s.first_time)),
                Cell::from(if r.reached_genesis { "✓完整" } else { "~部分" }),
            ]),
            None => Row::new(vec![
                Cell::from(format!("#{}", r.rank)),
                Cell::from(short(r.owner)),
                Cell::from("(未发现 SOL 入金)"),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(if r.reached_genesis { "✓完整" } else { "~部分" }),
            ]),
        }
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(13),
            Constraint::Length(16),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(5),
            Constraint::Length(17),
            Constraint::Min(6),
        ],
    )
    .header(
        Row::new(["持有", "钱包", "资金来源", "标签", "SOL", "笔数", "最早时间", "溯源"])
            .style(Style::new().bold().fg(Color::Cyan)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title(" 资金来源 (各持有人钱包最早的 SOL 入金) "));
    f.render_stateful_widget(table, area, &mut app.tables[3]);
}

fn draw_clusters(f: &mut Frame, area: Rect, app: &mut App) {
    if app.a.clusters.is_empty() {
        f.render_widget(
            Paragraph::new("\n  未发现共享资金来源。\n\n  提示: 加大 --top 与 --funding-scan 可以扩大溯源范围。")
                .block(Block::bordered().title(" 关联资金集群 ")),
            area,
        );
        return;
    }
    let rows = app.a.clusters.iter().map(|c| {
        let (mut kind, color) = match &c.label {
            Some(l) if crate::labels::is_exchange(l) => {
                (format!("{l} (交易所·弱关联)"), Color::Yellow)
            }
            Some(l) => (l.clone(), Color::Cyan),
            None => ("私人钱包·强关联".into(), Color::Red),
        };
        if c.time_span_secs.is_some_and(|s| s <= 6 * 3600) {
            kind.push_str(" ⏱同时段");
        }
        Row::new(vec![
            Cell::from(short(&c.source)),
            Cell::from(Span::styled(kind, Style::new().fg(color))),
            Cell::from(format!("{}", c.holders.len())),
            Cell::from(format!("{:.4}", c.total_sol)),
            Cell::from(
                c.holders
                    .iter()
                    .map(|h| short(h))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(13),
            Constraint::Length(22),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["来源", "性质", "人数", "SOL", "覆盖的持有人"])
            .style(Style::new().bold().fg(Color::Cyan)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title(
        " 关联资金集群 (同一来源给多个 Top 持有人打过钱 → 可能是关联钱包) ",
    ));
    f.render_stateful_widget(table, area, &mut app.tables[4]);
}
