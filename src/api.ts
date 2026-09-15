import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  Settings,
  LocalAccount,
  OAuthStart,
  OAuthPoll,
  CheckinLog,
  CreditReport,
  CreditSnapshot,
  SnapshotDiff,
  ImportItem,
  ImportReport,
  NetReport,
  NetRestoreReport,
  StealthStatus,
  JournalEvent,
  FreeModelsReport,
} from "./types";

export const listAccounts = () => invoke<Account[]>("list_accounts");

/**
 * 批量导入账号（「导入本机账号」与「登录新账号」共用）。
 * 已存在的账号（手机号或 token 相同）会被合并补全凭证，不会产生重复条目。
 */
export const importAccounts = (items: ImportItem[]) =>
  invoke<ImportReport>("import_accounts", { items });

/**
 * 导出全部账号到指定 JSON 文件（含 token / refresh_token，注意保密）。
 * 返回写入的文件路径。
 */
export const exportAccounts = (path: string) =>
  invoke<string>("export_accounts", { path });

/** 从导出文件导入账号：按手机号 / token 合并补全，不会产生重复条目 */
export const importAccountsFile = (path: string) =>
  invoke<ImportReport>("import_accounts_file", { path });

export const removeAccount = (id: string) =>
  invoke<void>("remove_account", { id });

export const checkinOne = (id: string) =>
  invoke<Account>("checkin_one", { id });

export const checkinAll = () => invoke<Account[]>("checkin_all");

/** 一键刷新：不打签到接口，重拉并持久化全部账号的积分快照 / 签到状态 / 积分余量 */
export const refreshAll = () => invoke<Account[]>("refresh_all");

/** 首选通道：读本机 WorkBuddy 登录文件（auth/*.info），含昵称与手机号 */
export const discoverLocalAccounts = () =>
  invoke<LocalAccount[]>("discover_local_accounts");

/** 「无感登录」第一步：申请 state + 授权链接（host 省略则国内版） */
export const oauthStart = (host?: string | null) =>
  invoke<OAuthStart>("oauth_start", { host: host ?? null });

/** 「无感登录」第二步：轮询授权结果；done=false 表示仍需继续轮询 */
export const oauthPoll = (loginId: string) =>
  invoke<OAuthPoll>("oauth_poll", { loginId });

/** 在系统默认浏览器打开链接（授权页） */
export const openExternal = (url: string) =>
  invoke<void>("open_external", { url });

export const getSettings = () => invoke<Settings>("get_settings");

export const saveSettings = (settings: Settings) =>
  invoke<Settings>("save_settings", { settings });

/** 原子应用设置；接管启停或换端口时会安全重启 WorkBuddy 与长驻 CLI host */
export const applySettings = (settings: Settings) =>
  invoke<Settings>("apply_settings", { settings });

/** 发一条测试通知，返回推送服务的原始响应 */
export const testNotify = (webhook: string) =>
  invoke<string>("test_notify", { webhook });

/** 是否已注册开机自启动（以操作系统为准） */
export const getAutostart = () => invoke<boolean>("get_autostart");

/** 开启/关闭开机自启动，返回落定后的真实状态 */
export const setAutostart = (enabled: boolean) =>
  invoke<boolean>("set_autostart", { enabled });

export const appVersion = () => invoke<string>("app_version");

export const getCheckinLogs = (limit?: number, accountId?: string) =>
  invoke<CheckinLog[]>("get_checkin_logs", {
    limit: limit ?? null,
    accountId: accountId ?? null,
  });

/** 清空签到日志：不传 accountId 则清空全部 */
export const clearCheckinLogs = (accountId?: string) =>
  invoke<void>("clear_checkin_logs", { accountId: accountId ?? null });

/**
 * 诊断 WorkBuddy 的全局网络配置（**只读**，不会改动任何文件）。
 * 扫的是：`~/.workbuddy/settings.json` / `~/.codebuddy/settings.json` 里的
 * `endpoint` 与 `env.CODEBUDDY_*`、launchd 全局环境变量、shell 启动脚本、本应用反代开关。
 */
export const netDiagnose = () => invoke<NetReport>("net_diagnose");

/**
 * 一键恢复：备份 → 清除调试残留键 → 取消 launchd 全局变量 → 关闭本地反代 → 复检。
 * 只碰「明确是调试写进去」的键，不认识的一律原样保留。
 */
export const netRestore = () => invoke<NetRestoreReport>("net_restore");

/** 在系统文件管理器里定位某个文件（用于查看备份） */
export const revealPath = (path: string) => invoke<void>("reveal_path", { path });

/** 查询智能接管状态（只读） */
export const stealthStatus = () => invoke<StealthStatus>("stealth_status");

/** 接管事件流（新的在前）：开启 / 关闭 / 开始使用账号 / 重启 / 错误 */
export const takeoverEvents = () => invoke<JournalEvent[]>("takeover_events");
/** 清空接管动态（不可恢复） */
export const clearTakeoverEvents = () => invoke<void>("takeover_events_clear");

/**
 * 「限流切换」支持的免费模型列表（积分倍率 x0.00，从网关动态拉取，缓存 1 小时）。
 * refresh=true 时忽略缓存强制重拉。
 */
export const freeModels = (refresh: boolean) =>
  invoke<FreeModelsReport>("free_models", { refresh });

/** 每日积分日报（新的在前）：窗口 = 上次结算 → 本次结算 */
export const creditReports = () => invoke<CreditReport[]>("credit_reports");

/** 清空日报历史（不影响积分台账与结算基线） */
export const clearCreditReports = () => invoke<void>("credit_reports_clear");

/**
 * **开启积分日报**：清空日报与快照历史 → 立即拉一次接口 → 把此刻读数落成
 * 第一条**系统快照**（兼作对比基线）。
 *
 * 有了这一步，用户不必等「次日首次打开应用」才看到第一条；开启即有一条基线，
 * 之后每天封口各推一条。**保留积分台账**（小时桶是真实采样，且是增量的基准）。
 */
export const enableCreditReports = () =>
  invoke<CreditReport>("credit_reports_enable");

/**
 * 取一次**当前累计读数**（「当前累计」按钮）。
 *
 * 会实时重拉接口、把逐包明细并进台账，并把这一刻的读数落成一条 `manual` 快照
 * （可与别的快照相减看增量）。但**不写日报历史** —— 日报列表只收完整自然日。
 */
export const settleCreditReport = () =>
  invoke<CreditReport>("credit_report_settle");

/** 积分快照（新的在前）：某一刻的读数，不是「一天的聚合」 */
export const creditSnapshots = () =>
  invoke<CreditSnapshot[]>("credit_snapshots");

/**
 * 每条快照与它**前面那一条**的差值增量。
 * 下标对应 {@link creditSnapshots} 返回数组里的位置；最老的一条没有前辈，不在结果里。
 */
export const creditSnapshotDiffs = () =>
  invoke<[number, SnapshotDiff][]>("credit_snapshot_diffs");

/** 清空全部快照（含系统锚点）。不影响日报历史与积分台账 */
export const clearCreditSnapshots = () => invoke<void>("credit_snapshots_clear");
