# 签到后自动发对话（点亮额度活跃地图）

服务端的「月活跃地图」只认**当天完成一次对话**，光签到不记录。多账号用户不可能挨个去聊天，
所以签到时顺手给每个账号发一条短消息，内容是按时段随机取的。

## 数据流

```
run_checkin_all(手动/网页点击)          run_checkin_cycle(启动 + 每 30 分钟)
        │                                        │
        ├── checkin_all_rows() 逐号签到           ├── maybe_daily_chat(eligible)   ← 先发对话，与签到窗口无关
        │                                        │        │
        └── maybe_daily_chat(accounts) ──────────┤        ├── 今天没发过的账号
                                                 ▼        ▼
                                     daily_chat::ensure_sent_today(accounts)
                                                 │
                        ┌────────────────────────┼─────────────────────────┐
                        ▼                        ▼                         ▼
                  load_logs()        串行发送 + 3~8 秒间隔        prune + 一次落盘
              已发过 → skipped  读 streak → chat_with_prompt    daily_chat_logs.json
                                  → 再读 streak（verify）
```

## 三条不变量

1. **每号每天最多一条**：`daily_chat_logs.json` 里当天有 `ok: true` 就不再发。
   失败行也记录（`ok: false`），但**不算数**——下一轮会自动重试。
2. **失败绝不影响签到**：对话结果只进 `chat` 汇总字段，签到行的 `result` / `error` 一个字段都不改。
3. **串行 + 间隔，且有发送下限**：账号之间隔 3~8 秒逐个发（同一天同一账号间隔恒定），
   **本地 09:00 前整轮不发**（`chat_after_hour`，默认 9；0 表示不限制）；同一进程内由
   `RunFlagGuard` 防重入。00:25 并发齐发点不亮活跃地图，正是这次要修的根因。

## 词库

`crates/wb-switch-core/src/modules/daily_chat.rs`：

| 时段 | 常量 | 何时命中 |
|---|---|---|
| 05:00–10:59 | `MORNING` | 本地时间 |
| 11:00–13:59 | `NOON` | |
| 14:00–17:59 | `AFTERNOON` | |
| 18:00–22:59 | `EVENING` | |
| 其余 | `LATE_NIGHT` | |
| 任意 | `COMMON` | 与上面每组拼接 |

抽签是 `FNV-1a(账号 id) ^ 当天序号 ^ 纳秒`：同一账号每天换句子，同一时刻的不同账号也不会撞同一句。
加句子直接往对应数组里塞一行即可，不用改逻辑。

## 文件索引

| 位置 | 作用 |
|---|---|
| `crates/wb-switch-core/src/modules/daily_chat.rs:140` | `phrase_for_key` 取词（可复现，便于测试） |
| `crates/wb-switch-core/src/modules/daily_chat.rs:260` | `ensure_sent_today` 时间窗拦截 + 串行发送（3~8 秒间隔）+ 回读自证 + 落盘 |
| `crates/wb-switch-core/src/modules/daily_chat.rs:97` | `should_chat_now` 发送下限判定（`after_hour = 0` 不限时） |
| `crates/wb-switch-core/src/modules/daily_chat.rs:105` | `chat_gap_ms` 账号间隔（3000~8000 ms，同输入恒定） |
| `crates/wb-switch-core/src/modules/daily_chat.rs:164` | `sent_today` 当天是否已发（只认成功） |
| `crates/wb-switch-core/src/modules/checkin.rs:902` | `maybe_daily_chat` 开关与名单过滤，并传出 `chat_after_hour` |
| `crates/wb-switch-core/src/modules/checkin.rs:892` | 手动 / 网页签到后的接线 |
| `crates/wb-switch-core/src/modules/checkin.rs:724` | 定时轮次的接线（在窗口判断之前） |
| `crates/wb-switch-core/src/modules/growth_tasks.rs:410` | `chat_with_prompt` 真正发请求 |
| `crates/wb-switch-core/src/modules/config.rs:309` | `chat_on_checkin` 默认 `true` |
| `crates/wb-switch-core/src/modules/config.rs:314` | `chat_after_hour` 默认 `9`（本地 09:00 前不发，0 = 不限制） |
| `src/pages/SettingsPage.tsx:711` | 设置页开关行 |
| `src/lib/types.ts:581` | 前端配置字段 |

## 避坑备忘

- **别把对话塞进签到循环里逐号发**：签到循环刻意有 3–8 秒随机间隔（防自动化指纹），
  对话塞进去会让「同时开启」变成排长队。必须**签完之后独立成轮，串行发送并保留 3~8 秒
  账号间隔**，别塞进签到账号循环 —— 也别退回「同一秒多号齐发」，那正是点不亮活跃地图的根因。
- **别去掉发送下限**：本地 09:00 前整轮不发（`chat_after_hour`，默认 9，0 = 不限制）。
  00:25 那一批落在服务端跨日结算边界上，发了也不点亮当天。
- **别用签到日志判断「今天发过没有」**：签到日志只在真正提交签到时写，白天手动签过的账号
  不会再有新行，会把对话漏掉。所以单独用 `daily_chat_logs.json`。
- **别在每个账号的任务里各自写盘**：4 个账号各自 `read → append → write` 会互相覆盖，
  统一在 `ensure_sent_today` 收尾时写一次。
- **别把失败行当成「已发」**：`sent_today` 只看 `ok: true`，否则一次网络抖动就整天不补。
- **别把 `verify` 当成发送结果**：`verify`（`new` / `same` / `unknown`）只是回读连登天数的自证，
  回读失败记 `unknown`，绝不影响 `ok`；`ok` 只看发送请求本身。
- **别忘了刷 token**：签到路径刷的是它手里那份副本，`ensure_sent_today` 拿到的账号可能还是旧
  token。每个账号先 `ensure_fresh_token`，否则「最该补对话的那批账号」会整片 401。
- **别让单号 panic 拖垮整轮**：每个账号的计算体跑在独立 `tokio::spawn` 里，`JoinError` 只记一次
  `failed`。改回 inline await 的话，一个 panic 会 unwind 整个后台签到 loop。
- **国际版没有成长中心**：`allows_auto_checkin` 已经用 `supports_checkin()` 挡掉了，
  新增入口时别绕开这个过滤，否则会朝不存在的接口发请求。
