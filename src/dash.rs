//! watch 模式的实时仪表盘：事件流 + 持仓面板 + 跟单动作。
//! 单任务渲染（watch 循环里按帧调用），无需跨任务共享状态。

use crate::trade::Executor;
use crate::types::{Side, WatchEvent, human, short};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::collections::VecDeque;

pub struct FeedItem {
    pub wallet: String,
    pub ev: WatchEvent,
}

pub struct DashView<'a> {
    pub feed: &'a VecDeque<FeedItem>,
    pub executor: Option<&'a Executor>,
    pub wallet_count: usize,
    pub sol_usd: Option<f64>,
    /// 事件流向上滚动的偏移（0 = 跟随最新）
    pub scroll: usize,
    pub ws_connected: bool,
}

pub fn draw(f: &mut Frame, v: &DashView) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(f.area());

    // ── 头部状态行
    let mut spans = vec![
        Span::styled(" 监控仪表盘 ", Style::new().bold().fg(Color::Cyan)),
        Span::raw(format!(
            "{} 个钱包  事件 {}  ",
            v.wallet_count,
            v.feed.len()
        )),
    ];
    if let Some(exec) = v.executor {
        let (spent, cap) = exec.spent_today();
        spans.push(Span::styled(
            if exec.is_live() { "LIVE " } else { "paper " },
            if exec.is_live() {
                Style::new().bold().fg(Color::Red)
            } else {
                Style::new().fg(Color::Green)
            },
        ));
        spans.push(Span::raw(format!("今日 {spent:.3}/{cap:.3} SOL  ")));
    }
    if let Some(r) = v.sol_usd {
        spans.push(Span::raw(format!("SOL ${r:.2}  ")));
    }
    spans.push(if v.ws_connected {
        Span::styled("●实时", Style::new().fg(Color::Green))
    } else {
        Span::styled("●连接中", Style::new().fg(Color::Yellow))
    });
    f.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);

    // ── 主体: 左事件流 / 右(持仓+动作)
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[1]);
    draw_feed(f, body[0], v);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(body[1]);
    draw_positions(f, right[0], v);
    draw_actions(f, right[1], v);

    let help = if v.scroll > 0 {
        format!(" q 退出 | ↑↓ 滚动 (回看 {} 条, End 回到最新)", v.scroll)
    } else {
        " q 退出 | ↑↓ 滚动事件流".to_string()
    };
    f.render_widget(
        Paragraph::new(help).style(Style::new().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn draw_feed(f: &mut Frame, area: ratatui::layout::Rect, v: &DashView) {
    let visible = area.height.saturating_sub(2) as usize;
    let total = v.feed.len();
    let end = total.saturating_sub(v.scroll);
    let start = end.saturating_sub(visible);
    let lines: Vec<Line> = v
        .feed
        .iter()
        .skip(start)
        .take(end - start)
        .map(|item| {
            let e = &item.ev.event;
            let (tag, color) = match e.side {
                Side::Buy => ("买", Color::Green),
                Side::Sell => ("卖", Color::Red),
                Side::TransferIn => ("入", Color::Cyan),
                Side::TransferOut => ("出", Color::Yellow),
            };
            let value = if e.sol_amount > 0.0 {
                format!(" {:.3}◎", e.sol_amount)
            } else if e.usd_amount > 0.0 {
                format!(" ${:.0}", e.usd_amount)
            } else {
                String::new()
            };
            let t = e
                .time
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
                .unwrap_or_else(|| "--:--:--".into());
            Line::from(vec![
                Span::styled(format!("{t} "), Style::new().fg(Color::DarkGray)),
                Span::raw(format!("{} ", short(&item.wallet))),
                Span::styled(format!("{tag} "), Style::new().bold().fg(color)),
                Span::raw(format!("{} {}", human(e.token_amount), item.ev.token_disp())),
                Span::styled(value, Style::new().fg(color)),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(format!(" 事件流 ({total}) "))),
        area,
    );
}

fn draw_positions(f: &mut Frame, area: ratatui::layout::Rect, v: &DashView) {
    let Some(exec) = v.executor else {
        f.render_widget(
            Paragraph::new("  (未开启跟单)").block(Block::bordered().title(" 持仓 ")),
            area,
        );
        return;
    };
    let positions = exec.positions_view();
    let mut total_cost = 0.0;
    let mut total_value = 0.0;
    let rows: Vec<Row> = positions
        .iter()
        .map(|(mint, raw, cost, value)| {
            total_cost += cost;
            let (val_str, pnl_cell) = match value {
                Some(val) => {
                    total_value += val;
                    let pnl = val - cost;
                    let pct = if *cost > 0.0 { pnl / cost * 100.0 } else { 0.0 };
                    let style = if pnl >= 0.0 {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new().fg(Color::Red)
                    };
                    (
                        format!("{val:.4}"),
                        Cell::from(Span::styled(format!("{pct:+.1}%"), style)),
                    )
                }
                None => ("...".into(), Cell::from("-")),
            };
            Row::new(vec![
                Cell::from(short(mint)),
                Cell::from(human(*raw as f64)),
                Cell::from(format!("{cost:.4}")),
                Cell::from(val_str),
                pnl_cell,
            ])
        })
        .collect();
    let title = if total_value > 0.0 {
        let pnl = total_value - total_cost;
        format!(
            " 持仓 {} 个  成本 {total_cost:.4}◎ 现值 {total_value:.4}◎ ({}{:.4}◎) ",
            positions.len(),
            if pnl >= 0.0 { "+" } else { "" },
            pnl
        )
    } else {
        format!(" 持仓 {} 个  成本 {total_cost:.4}◎ ", positions.len())
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(13),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(7),
        ],
    )
    .header(
        Row::new(["代币", "数量(raw)", "成本◎", "现值◎", "盈亏"])
            .style(Style::new().bold().fg(Color::Cyan)),
    )
    .block(Block::bordered().title(title));
    f.render_widget(table, area);
}

fn draw_actions(f: &mut Frame, area: ratatui::layout::Rect, v: &DashView) {
    let Some(exec) = v.executor else {
        f.render_widget(Block::new().borders(Borders::ALL).title(" 跟单动作 "), area);
        return;
    };
    let visible = area.height.saturating_sub(2) as usize;
    let mut recent: Vec<&String> = exec.recent_actions().rev().take(visible).collect();
    recent.reverse();
    let lines: Vec<Line> = recent
        .into_iter()
        .map(|s| {
            let color = if s.contains('✗') || s.contains('⚠') {
                Color::Yellow
            } else if s.contains("止盈") || s.contains("跟买") {
                Color::Green
            } else if s.contains("止损") {
                Color::Red
            } else {
                Color::Reset
            };
            Line::from(Span::styled(s.clone(), Style::new().fg(color)))
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" 跟单动作 ")),
        area,
    );
}
