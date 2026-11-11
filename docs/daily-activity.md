# 国际版每日活跃会话（网页通道）

> 状态：已实现（编译与单测通过；真机会话验证需手动触发一次，会消耗账号额度）
> 代码：`crates/wb-switch-core/src/modules/web_activity.rs`
>
> **风险声明**：该链路为**社区逆向**出来的非官方接口，路径与报文随时可能变更；
> ACP 的 `session/set_model` 已从规范移除，调用失败时只能按会话默认模型跑完这一轮。

## 为什么需要它

国际版**没有签到**。`/billing/meter/daily-checkin` 对国际版账号恒返回
`10001 签到活动未开启`，所以 `variantSupportsCheckin()` 对 `ai` 档位返回 false，
账号页也不展示签到入口。

但国际版有一条独立的奖励链路：**在网页端建一条 agent 会话并把它真正跑完**，
官方按「每日活跃」发放 30/50 积分。这条链路与签到完全无关：

| | 签到 | 每日活跃 |
|---|---|---|
| 接口 | `POST /billing/meter/daily-checkin` | `POST /console/as/conversations/` + ACP |
| 准入门槛 | 服务端 `active` 开关 | 无开关，会话 `completed` 即计分 |
| 适用档位 | 国内版 | 国际版 |
| 鉴权 | 桌面端完整指纹 | 仅 `Authorization: Bearer` + `X-User-Id` |

## 执行链路

一次打卡 = 一个会话的五步：

1. **建会话**
   `POST https://www.workbuddy.ai/console/as/conversations/`

   ```json
   {
     "prompt": "Hi",
     "model": "hy3",
     "conversationOrigin": "workbuddy-app",
     "plugins": [{ "name": "weixinpay", "marketplace": "codebuddy-builtin" }]
   }
   ```

   建会话只是**排队**：不接沙箱、不请求这一轮，agent 永远不会跑。

2. **取沙箱**
   `GET .../conversations/{id}/session` → `link`（E2B 沙箱地址）、`token`、
   `sessionId`、`cwd`。

3. **驱动 ACP**（JSON-RPC over streamable HTTP）
   - `GET <link>`，`Accept: text/event-stream`，从响应头取 `Acp-Connection-Id`
   - `POST <link>` 依次发 `initialize` → `session/load` → `session/set_model`
     → `session/prompt`，每条都带 `Acp-Connection-Id`
   - 后台持续读 SSE 流，避免连接被判空闲

4. **轮询到完成**
   `GET .../conversations/{id}`，直到 `status == "completed"`（单轮上限 120 秒）。

5. **收尾**
   成功 `POST .../stop`（会话留在历史里，只改状态）；失败 `POST .../delete`
   避免网页端堆满「Hi」。

## 模型与成本

按候选列表顺序尝试，成功即停。免费模型恒在列表里，付费备选只有配置
`allow_paid_model: true` 时才会追加：

| 模型 | 倍率 | 上下文 | 实测单次成本 |
|---|---|---|---|
| `hy3` | x0.00 | 192k | **0.00** |
| `deepseek-v4.1-flash` | x0.11 | 300k | 9.36 |

打卡只需要一句「Hi」加一段短回复，默认只试免费的 `hy3`；`hy3` 失败（限流/下线）
且用户已显式授权付费模型时，才顺延到 `deepseek-v4.1-flash`。

**注意**：网页通道无法单独下调思考档位。ACP 只提供 `session/set_model`，没有
reasoning effort 方法，所以 `deepseek-v4.1-flash` 的 `onlyReasoning: true` /
`defaultEffort: high` 改不动——省钱只能换模型，不能调参。

多账号连续执行时免费模型可能触发上游限流，按候选顺序顺延即可（付费备选需先授权）。

## 配置与接口

开关文件：`~/.wb-switch/daily_activity_config.json`，默认
`{"enabled": false, "allow_paid_model": false}`。配置缺失、JSON 损坏或字段非布尔
一律按 `false` 处理——这条链路会真的建会话，必须由用户显式打开。
**时间段不单独配置**，跟随签到配置的 `checkin_start` / `checkin_end`：两端都合法
且 `start < end` 时按窗口判定；未设置窗口时退化为「`chat_after_hour` 之后」
（默认 9 点），不会被当成完全不限。

去重状态：`~/.wb-switch/daily_activity_state.json`，记录 `uid → 最后成功日期`。
当天已完成的账号直接 `skipped`，不发任何请求。

| 接口 | 说明 |
|---|---|
| `GET /api/activity/config` | 读开关 |
| `POST /api/activity/config` | 写开关 |
| `GET /api/activity/status` | 各账号今日状态（账号页 chip 用） |
| `POST /api/activity/daily` | 单账号手动跑（不受开关与时间段限制） |
| `POST /api/activity/daily/all` | 所有国际版账号手动跑一轮 |

调度：`lib.rs`（Tauri）与 `main.rs`（server）各起一个后台循环，启动即跑一轮，
之后每 30 分钟轮询一次。只记录「真跑了」和「失败」，`skipped` 不刷日志。
单账号入口带防重入标志：手动触发与后台轮次撞在一起时，后到的那个直接返回
`{"ok": false, "busy": true}`，不会双倍消耗额度。账号之间按哈希间隔错开 3~8 秒，
最后一个不等待。

## 不切换账号

整个模块**没有任何 `switch` 调用**。它只读账号库里该账号自己的 `access_token`
和 `uid`，直接打网页通道。当前 WorkBuddy 登录态不受影响，账号库里有多少个国际版
账号就能跑多少个，与当前选中哪个账号无关。

## 已知限制

**沙箱域名的 DNS**：`*.sandbox.cloudstudio.club` 会被部分代理的 fake-ip 模式解析成
保留段地址（RFC 2544 基准测试网段），TCP 能连上但 TLS 握手直接被掐断
（`UNEXPECTED_EOF_WHILE_READING`）。模块内置 DoH 兜底（`dns.alidns.com` 查 A 记录 +
`ClientBuilder::resolve()` 固定真实 IP），不依赖用户配置；DoH 也查不到时直接返回
可读错误提示更换网络，不再静默降级成注定失败的连接。

**活跃会话上限**：上游对单账号同时存在的 `working` 会话有硬上限 10 条，攒满后建会话
直接 429 / 10105。命中上限时本模块**不清理任何既有会话**——`status` 无法区分本模块
建的会话与用户此刻正在用的会话，宁可当天活跃失败也不得中断用户会话；此时返回
`limitBlocked` 与可读提示，由用户自行到网页端结束旧对话。

**协议是逆向的**：这套链路依赖上游私有协议，上游收紧非官方客户端识别时可能失效。
失效后需要对照网页端抓包更新 `web_activity.rs`。
