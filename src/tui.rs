//! ratatui 交互界面：概览 / 持有人盈亏 / 资金流向 / 关联集群 四个标签页。

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

const TABS: [&str; 4] = ["概览", "持有人盈亏", "资金流向", "关联集群"];

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Balance,
    Unrealized,
    Realized,
}

pub struct App<'a> {
    a: &'a Analysis,
    tab: usize,
    tables: [TableState; 4],
    sort: SortBy,
    pnl_by_owner: HashMap<&'a str, &'a HolderPnl>,
    flow_rows: Vec<FlowRow<'a>>,
}

struct FlowRow<'a> {
    owner: &'a str,
    rank: usize,
    source: Option<&'a crate::types::FundingSource>,
    reached_genesis: bool,
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
            });
        }
        for s in f.sources.iter().take(5) {
            flow_rows.push(FlowRow {
                owner: &f.owner,
                rank,
                source: Some(s),
                reached_genesis: f.reached_genesis,
            });
        }
    }

    let mut app = App {
        a,
        tab: 0,
        tables: Default::default(),
        sort: SortBy::Balance,
        pnl_by_owner,
        flow_rows,
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
                KeyCode::Char('s') if app.tab == 1 => {
                    app.sort = match app.sort {
                        SortBy::Balance => SortBy::Unrealized,
                        SortBy::Unrealized => SortBy::Realized,
                        SortBy::Realized => SortBy::Balance,
                    };
                }
                _ => {}
            }
        }
    }
}

impl App<'_> {
    fn row_count(&self) -> usize {
        match self.tab {
            1 => self.a.holders.len().min(100),
            2 => self.flow_rows.len(),
            3 => self.a.clusters.len(),
            _ => 0,
        }
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
        .map(|p| format!("{p:.10} SOL"))
        .unwrap_or_else(|| "-".into());
    let header = Line::from(vec![
        Span::styled(" SPL Token 分析 ", Style::new().bold().fg(Color::Cyan)),
        Span::raw(format!(
            "{}  供应 {}  持有人 {}{}  价格 {}",
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

    match app.tab {
        0 => draw_overview(f, chunks[2], app),
        1 => draw_holders(f, chunks[2], app),
        2 => draw_flow(f, chunks[2], app),
        _ => draw_clusters(f, chunks[2], app),
    }

    let help = match app.tab {
        1 => " q 退出 | ←→/Tab 切换标签 | ↑↓ 滚动 | s 切换排序",
        _ => " q 退出 | ←→/Tab 切换标签 | ↑↓ 滚动",
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

fn draw_holders(f: &mut Frame, area: Rect, app: &mut App) {
    let mut holders: Vec<&crate::types::Holder> = app.a.holders.iter().take(100).collect();
    if app.sort != SortBy::Balance {
        holders.sort_by(|x, y| {
            let key = |h: &crate::types::Holder| {
                app.pnl_by_owner
                    .get(h.owner.as_str())
                    .map(|p| match app.sort {
                        SortBy::Unrealized => p.unrealized_sol.unwrap_or(f64::NEG_INFINITY),
                        _ => p.realized_sol,
                    })
                    .unwrap_or(f64::NEG_INFINITY)
            };
            key(y).total_cmp(&key(x))
        });
    }

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
            Constraint::Min(8),
        ],
    )
    .header(
        Row::new([
            "#", "地址", "余额", "占比%", "买入量", "卖出量", "转入量", "转出量", "均价SOL",
            "已实现", "浮动", "状态",
        ])
        .style(Style::new().bold().fg(Color::Cyan)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title(format!(
        " Top 持有人 (SOL 计价盈亏, 排序: {sort_name}, ~ 表示历史不完整) "
    )));
    f.render_stateful_widget(table, area, &mut app.tables[1]);
}

fn draw_flow(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.flow_rows.iter().map(|r| {
        match r.source {
            Some(s) => Row::new(vec![
                Cell::from(format!("#{}", r.rank)),
                Cell::from(short(r.owner)),
                Cell::from(format!("← {}", short(&s.source))),
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
    f.render_stateful_widget(table, area, &mut app.tables[2]);
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
    f.render_stateful_widget(table, area, &mut app.tables[3]);
}
