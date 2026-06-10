# tokens-analysis

Solana SPL Token 筹码结构与资金流向分析工具（Rust + TUI）。

## 功能

- **筹码结构**：全量扫描持有人（`getProgramAccounts` + dataSlice，16 万持有人级别可用），
  集中度指标（Top1/10/20/100、HHI）、巨鲸/大户/中户/散户分层
- **持有人盈亏**：对 Top N 持有人重放代币账户交易历史，从 pre/post 余额差识别
  买入/卖出/转入/转出（协议无关，Raydium/Pump/Jupiter 通用），计算 SOL 计价的
  成本均价、已实现盈亏、浮动盈亏与状态（浮盈/浮亏/已清仓）
- **价格发现**：从最大 AMM 池子金库的最近成交直接推导最新价格；
  从 Raydium SOL/USDC 池推导 SOL/USD 汇率，价格同时以 USD 展示
- **资金流向**：追溯每个持有人钱包最早的 SOL 入金来源，标注已知交易所热钱包，
  `--hops 2` 继续追上游来源的来源；识别多个持有人共享的资金来源
  （关联钱包集群，区分交易所弱关联、私人钱包强关联、同时段集中注资的钱包农场）
- **代币互转图**：从转账事件解析对手方，聚合 Top 持有人之间的筹码搬运路线
  （大户互倒筹码 = 同一控制人的强信号）
- **筹码快照对比**：`--snapshot` 保存持有人快照，`--diff` 对比出谁加仓、
  谁减仓、谁新进、谁清仓
- **代币 symbol**：自动解析 Metaplex / Token-2022 元数据，全界面显示代币符号
- **展示**：ratatui TUI 四个标签页（概览/持有人盈亏/资金流向/关联集群），
  持有人页按 Enter 查看单钱包交易明细；`--no-tui` 或管道输出时自动切换为纯文本报告

## 使用

```bash
cargo build --release

# RPC 解析顺序: --rpc / SOLANA_RPC_URL → ~/.config/solana/cli/config.yml → 公共节点
./target/release/tokens-analysis <MINT>

# 常用参数
./target/release/tokens-analysis <MINT> \
  --top 15           # 深度分析的持有人数量
  --tx-limit 80      # 每个持有人扫描的交易数
  --funding-scan 25  # 资金溯源扫描的最早交易数
  --hops 2           # 资金溯源跳数（2 = 追上游来源的来源）
  --no-tui           # 纯文本报告

# 只分析指定钱包（跳过全量扫描）
./target/release/tokens-analysis <MINT> --owners <WALLET1>,<WALLET2>
```

建议使用支持 `getProgramAccounts` 的 RPC（如 [Triton One](https://docs.triton.one)）。
公共节点会回退到 Top20 模式且 `getTokenLargestAccounts` 经常被限流。

## 聪明钱发现 → 跟单闭环

```bash
# 1. 分析代币，按评分导出聪明钱（ROI/利润/活跃度/数据完整度加权，0-100）
tokens-analysis analyze <MINT> --top 30 --export-smart-money smart.jsonl --smart-min-score 60

# 2. 直接监控这批钱包并跟单
tokens-analysis watch --wallets-file smart.jsonl --copy
```

## 监控与跟单

跟买前自动做**安全检查**：mint/freeze authority 未放弃、Token-2022 转账税 >1%、
transfer hook、永久代理、默认冻结账户——任一命中即跳过该代币并写审计日志
（`--allow-risky` 可关闭，不建议）。

```bash
# 监控钱包动向（只打印事件流）
tokens-analysis watch --wallets <W1>,<W2>

# paper 跟单：记录决策与报价，不发交易（默认）
tokens-analysis watch --wallets <W1> --copy

# 真实下单（启动时需确认；务必先跑 paper 验证）
tokens-analysis watch --wallets <W1> --copy --live \
  --buy-sol 0.05 --max-daily-sol 0.5 --slippage-bps 300 --min-trigger-sol 0.5

# 止盈止损：现值达成本 2 倍清仓 / 跌到一半清仓（paper 模式也生效）
tokens-analysis watch --wallets <W1> --copy --take-profit 2.0 --stop-loss 0.5

# 买卖执行后推送通知
tokens-analysis watch --wallets <W1> --copy --notify desktop     # macOS 通知中心
tokens-analysis watch --wallets <W1> --copy --notify telegram    # env TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID

# 筹码快照与迁移对比
tokens-analysis analyze <MINT> --snapshot          # 保存快照
tokens-analysis analyze <MINT> --diff              # 对比最近一次快照
```

监控默认走 **WebSocket 实时推送**（logsSubscribe，亚秒级延迟，断线自动重连），
WS 端点由 RPC URL 自动推导；`--no-ws` 可强制回退轮询模式。

`--tui` 进入**实时仪表盘**：左侧滚动事件流，右上持仓面板（实时现值/浮动盈亏，
Jupiter 报价估值），右下跟单动作记录；顶部状态栏显示今日额度、SOL 价格、连接状态。
`↑↓` 回看历史事件，`q` 退出。

```bash
tokens-analysis watch --wallets-file smart.jsonl --copy --tui \
  --take-profit 2 --stop-loss 0.5
```

仓位持久化在 `positions-paper.json`（live 模式 `positions.json`），重启自动恢复。
止盈止损用 Jupiter 报价计算持仓的**真实可变现价值**（天然包含流动性深度与价格冲击），
每 `--price-check-interval` 秒巡检一次。

执行路径：Jupiter Swap API 报价/组交易 → 本地 ed25519 签名（签名前校验
fee payer 是本钱包）→ `sendTransaction`（含 preflight 模拟）→ 轮询确认。
密钥默认读 `~/.config/solana/id.json`（Solana CLI 格式），可用 `--keypair` 指定。

安全护栏：默认 paper；`--live` 启动需输入 yes；单笔固定金额 + 每日总额上限 +
滑点上限 + 触发阈值（过滤灰尘信号，稳定币买入按 SOL/USD 汇率折算）；
跟卖只卖本工具买入的仓位、比例跟随目标钱包；所有决策写 JSONL 审计日志。

⚠ **风险提示**：跟单交易使用真实资金，行情剧烈时滑点保护可能使交易失败或
成交价劣于预期；目标钱包可能反向操作或洗盘。请只用可承受损失的资金，
从小额（如 `--buy-sol 0.01`）开始验证。

## 数据口径说明

- 盈亏以 **SOL 计价**；稳定币（USDC/USDT）腿的交易单独累计为 USD 金额，不折算进 SOL 成本
- 状态前缀 `~` 表示历史被截断或存在成本未知的筹码（转入/稳定币买入），数值为近似
- 纯转入仓位（成本完全未知）不计算浮动盈亏，避免把市值当利润
- 资金溯源最多回翻 5000 笔签名，`✓完整` 表示已到达钱包创建时刻，`~部分` 表示窗口有限
