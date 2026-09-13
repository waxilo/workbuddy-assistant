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
}

export interface Settings {
  default_base_url: string;
  auto_checkin_on_start: boolean;
  /** 每天定时自动签到（应用需保持运行） */
  schedule_enabled: boolean;
  /** `HH:MM`，24 小时制 */
  schedule_time: string;
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
  /** 多账号风控预防：批量签到时在账号之间加入随机间隔 */
  stagger_checkin: boolean;
  /** 随机间隔上限（秒），实际在 2..=max 之间取值 */
  stagger_max_seconds: number;
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

/** 「限流切换」支持的免费模型列表（从网关动态拉取） */
export interface FreeModelsReport {
  models: string[];
  /** fetched = 刚从网关拉取；cache = 1 小时缓存内；fallback = 拉取失败用内置兜底 */
  source: "fetched" | "cache" | "fallback";
}
