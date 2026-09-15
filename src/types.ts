export interface CheckinRecord {
  success: boolean;
  already: boolean;
  inactive: boolean;
  message: string;
  credit: number | null;
  balance: number | null;
  streak: number | null;
  host: string | null;
  at: string;
  code: number | null;
}

export interface Account {
  id: string;
  name: string;
  phone: string | null;
  token: string;
  /** 续签用的 refresh token（有它才能自动续期） */
  refresh_token: string | null;
  /** access token 过期时间（毫秒）；null 表示未知 */
  expires_at: number | null;
  base_url: string | null;
  created_at: string;
  last: CheckinRecord | null;
  /** 积分快照（持久化）：剩余积分 + 最早过期时间 + 拉取时刻；路由与展示共用 */
  credit_snapshot: CreditSnapshot | null;
  /** 服务端「今日是否已签到」的真实状态（持久化）；null = 未查询/失败 */
  checked_today: boolean | null;
}

export interface Settings {
  default_base_url: string;
  auto_checkin_on_start: boolean;
  /** 每天定时自动签到（应用需保持运行） */
  schedule_enabled: boolean;
  /** `HH:MM`，24 小时制 */
  schedule_time: string;
  /** 定时签到的随机时间窗（分钟）：当天实际触发 = `schedule_time` + `[0, 窗口]` 内随机；0 = 关闭随机 */
  schedule_window_minutes: number;
  /** 通知总开关 */
  notify_enabled: boolean;
  /** 通知 webhook，形如 https://…/hook/<key> */
  notify_webhook: string;
  /** 定时签到后推送 */
  notify_on_schedule: boolean;
  /** 手动「全部签到」后推送 */
  notify_on_manual: boolean;
  /**
   * 智能接管总开关（WorkBuddy 专用反代）：开启 = 监听 127.0.0.1 并把 WorkBuddy
   * 端点指向它，按积分过期时间优先路由；关闭 = 停止监听并摘掉端点。无鉴权 Key。
   */
  proxy_enabled: boolean;
  /** 反代监听端口 */
  proxy_port: number;
  /** 扣费备选账号 id 列表（多选）：反代只在这批账号里选号扣费；空 = 全部可用 */
  billing_account_ids: string[];
  /** 限流无感切换生效的模型 id 列表（多选）：这些模型触发 429 时自动换备用账号重发；
   *  0 积分免费模型恒生效无需勾选，这里只存用户额外勾选的付费模型；空 = 仅免费模型 */
  rate_limit_models: string[];
  /** 限流（429）时是否在**同一会话内**换备用账号。
   *  true = 无感续跑，但同一会话会出现中途换凭证；false = 防御优先，429 原样透传 */
  failover_on_rate_limit: boolean;
  /** 多账号风控预防：批量签到时在账号之间加入随机间隔 */
  stagger_checkin: boolean;
  /** 随机间隔上限（秒），实际在 2..=max 之间取值 */
  stagger_max_seconds: number;
  /** 批量签到时随机打乱账号顺序（只影响请求次序，列表顺序不变） */
  shuffle_checkin_order: boolean;
  /** 手动「全部签到」也加账号间隔（避免手动路径成为唯一的瞬时连发入口） */
  manual_stagger: boolean;
  /** 手动「全部签到」的间隔上限（秒），实际在 2..=max 之间取值 */
  manual_stagger_max_seconds: number;
  /** 每日积分日报：应用常驻时每天在 report_time 结算一次 */
  report_enabled: boolean;
  /** 日报结算时刻，24 小时制 HH:MM（默认 12:00） */
  report_time: string;
  /** 日报是否推送到 webhook（复用通知总开关与地址） */
  notify_on_report: boolean;
}

/**
 * 直接读本机 WorkBuddy 登录文件（auth/*.info）得到的账号。
 * 一次就能拿到 token + 昵称 + 手机号，是首选通道。
 */
export interface LocalAccount {
  token: string;
  /** 续签用的 refresh token */
  refresh_token: string | null;
  source: string;
  host: string | null;
  /** 官方文件里的 account.uid */
  uid: string | null;
  nickname: string | null;
  phone: string | null;
  /** access token 过期时间（毫秒时间戳） */
  expires_at: number | null;
  /** 来自官方固定文件或带 lastLogin 标记，即当前实际登录的账号 */
  is_current: boolean;
  file: string;
}

/** 一次导入请求：token 必填，其余为可直接预填的元信息 */
export interface ImportItem {
  token: string;
  host?: string | null;
  name?: string | null;
  phone?: string | null;
  refresh_token?: string | null;
  expires_at?: number | null;
}

/** 批量导入结果：已存在的账号会被合并补全而不是跳过 */
export interface ImportReport {
  added: number;
  updated: number;
}

/** 「无感登录」第一步的返回：授权链接与本次会话 id */
export interface OAuthStart {
  login_id: string;
  verification_uri: string;
  host: string;
  expires_in: number;
}

/** 「无感登录」轮询结果：done=false 表示还在等用户授权（不是错误） */
export interface OAuthPoll {
  done: boolean;
  token: string | null;
  /** 续签用的 refresh token（授权接口一并返回） */
  refresh_token: string | null;
  host: string | null;
  uid: string | null;
  nickname: string | null;
  phone: string | null;
  /** access token 过期时间（毫秒时间戳） */
  expires_at: number | null;
  error: string | null;
}

/** 积分快照（持久化到 accounts.json）：路由与展示共用的真实积分画像 */
export interface CreditSnapshot {
  /** 剩余积分（get-user-resource 汇总，取不到为 null） */
  credits: number | null;
  /** 还有余量的资源包里最早的重置/过期时间（毫秒时间戳）；null = 未知 */
  earliest_expiry_ms: number | null;
  /** 本次拉取时刻（本地时间串），用于判断快照是否过期 */
  fetched_at: string | null;
}

export interface CheckinLog {
  id: string;
  account_id: string;
  account_name: string;
  account_phone: string | null;
  at: string;
  success: boolean;
  already: boolean;
  inactive: boolean;
  code: number | null;
  message: string;
  host: string | null;
  credit: number | null;
  balance: number | null;
}

/** 「网络急救」里的一个可疑点 */
export interface NetIssue {
  id: string;
  /** 分类：配置文件 / launchd 全局环境 / shell 启动脚本 / 本应用反代 */
  scope: string;
  /** 具体位置：文件路径、变量名或设置项 */
  target: string;
  value: string;
  /**
   * block = 会让网络不通；warn = 残留但当前不影响连通性；
   * ok = 正常状态（例如本应用智能接管正在工作），不算问题
   */
  level: "block" | "warn" | "ok";
  note: string;
  /** 是否属于「一键恢复」能自动处理的范畴 */
  fixable: boolean;
}

export interface NetReport {
  /** 没有 block 级问题时为 true */
  healthy: boolean;
  issues: NetIssue[];
  /** 已扫描的位置 */
  scanned: string[];
}

export interface NetStep {
  action: string;
  ok: boolean;
  detail: string;
}

export interface NetRestoreReport {
  steps: NetStep[];
  /** 已备份的文件路径 */
  backups: string[];
  /** 本地反代是否被本次恢复关掉 */
  proxy_disabled: boolean;
  report: NetReport;
}

/** 智能接管的当前状态 */
export interface StealthStatus {
  /** 设置里是否开启 */
  enabled: boolean;
  /** 端点是否真的写进 WorkBuddy 配置了 */
  installed: boolean;
  /** 租约是否新鲜（心跳还在跳） */
  alive: boolean;
  port: number;
  url: string;
  /** 人话说明当前状态与下一步该做什么 */
  note: string;
}

/** 接管事件流的一条记录（takeover-journal.jsonl） */
export interface JournalEvent {
  at_ms: number;
  at: string;
  /** install / uninstall / route_start / restart_workbuddy / proxy_upstream_error / … */
  event: string;
  detail: string;
}

/** 「限流切换」模型列表中的单个模型 */
export interface ModelInfo {
  /** 模型 id，如 hy3 / hy3-x / deepseek-v3 … */
  id: string;
  /** 是否 0 积分免费模型（恒生效、UI 锁定勾选不可取消） */
  free: boolean;
  /** 积分倍率原始串（如 "x0.00" / "x0.05"），仅展示用 */
  multiplier: string;
}

/** 「限流切换」支持的模型列表（全模型，从网关动态拉取） */
export interface FreeModelsReport {
  /** 免费排前、其余按 id 排序 */
  models: ModelInfo[];
  /** fetched = 刚从网关拉取；cache = 1 小时缓存内；fallback = 拉取失败用内置兜底 */
  source: "fetched" | "cache" | "fallback";
}

/** 积分日报里的一行：某个账号在窗口内的消耗与新增 */
export interface CreditReportAccount {
  account_id: string;
  name: string;
  phone: string | null;
  /** 当天消耗（Σ 当天小时桶；下标 = 小时） */
  consumed: number;
  /** 当天新增（Σ 当天小时桶，含签到发的包） */
  gained: number;
  /** 结算时点的剩余积分（取不到为 null） */
  balance: number | null;
  /** 结算时点仍在计量的资源包个数 */
  packages: number;
  /** 该账号在这一天的 24 个消耗桶（下标 = 小时，未采样的小时为 0） */
  hours_consumed: number[];
  /** 该账号在这一天的 24 个新增桶 */
  hours_gained: number[];
}

/** 某一天里某个小时的合计（只列有数据的时点） */
export interface HourTotal {
  /** 小时（0–23） */
  hour: number;
  consumed: number;
  gained: number;
}

/**
 * 一条每日积分日报。口径 = **自然日 00:00–24:00**。
 *
 * 「消耗」与「新增」都取自接口里资源包的**累计**字段（`CapacityUsed` / `CapacitySize`）
 * 的增量，按采样时刻归入所属小时，所以多个客户端同时消耗都能算进来，
 * 不需要按请求归因，并发也不会算错。
 *
 * 小时桶在采样时就已归位，因此「按天」和「按小时」是同一份数据的两种聚合：
 * 小时之和恒等于当天合计，相邻两天可直接相加。
 */
export interface CreditReport {
  /** 结算日 YYYY-MM-DD（列表按它倒序） */
  date: string;
  /** 该条日报的生成时刻（当天 12:00 或手动结算） */
  generated_at: string;
  /** 窗口起点，固定为当天 00:00:00 */
  window_from: string;
  /** 窗口终点：当天为生成时刻（还没走完），封口后为次日 00:00:00 */
  window_to: string;
  /** 自然日是否已走完 */
  sealed: boolean;
  /** 0 = 逐小时数据实测可用；1 = 自然日口径上线时对当天做的补算，无小时明细 */
  granularity: number;
  accounts: CreditReportAccount[];
  total_consumed: number;
  total_gained: number;
  /** 全部账号剩余积分合计；一个都取不到时为 null（不谎报 0） */
  total_balance: number | null;
  /** 当天每小时合计（只列有数据的时点） */
  hours: HourTotal[];
}
