import type { Account, CheckinLog } from "./types";

/**
 * 跨页面共享的 UI 基础件：类型、展示助手与状态徽标。
 * 只放「无业务依赖」的东西，任何页面都能安全引用。
 */

export type Toast = { kind: "ok" | "err" | "info"; text: string } | null;

/** 自研确认框的请求描述（resolve 由 App 统一收口） */
export type ConfirmReq = {
  title: string;
  body?: string;
  okText?: string;
  danger?: boolean;
  resolve: (ok: boolean) => void;
};

export function maskToken(t: string): string {
  if (t.length <= 12) return "•".repeat(t.length);
  return t.slice(0, 6) + "…" + t.slice(-4);
}

/** 路径取文件名（兼容 Windows 反斜杠） */
export function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

/** 剩余积分展示：最多两位小数且不留尾随 0（接口给的是 805.14000097 这种精度） */
export function formatCredits(v?: number | null): string {
  if (v == null) return "—";
  return String(Math.round(v * 100) / 100);
}

/** 手机号脱敏（纯展示）：11 位纯数字按 138****1234 处理，其它字符串原样返回 */
export function maskPhone(s: string): string {
  return /^\d{11}$/.test(s) ? s.slice(0, 3) + "****" + s.slice(7) : s;
}

/// 账号在日志/筛选中的展示名：名称 + 手机号（手机号缺失时省略）。
/// 名称本身是手机号时同样脱敏；过滤/匹配请直接用原始字段，不要经过这里。
export function accountLabel(name: string, phone?: string | null): string {
  return phone ? `${maskPhone(name)}（${maskPhone(phone)}）` : maskPhone(name);
}

/** 解析「YYYY-MM-DD HH:MM:SS」为 Date；格式不符返回 null */
function parseAt(at?: string | null): Date | null {
  if (!at) return null;
  const d = new Date(at.replace(" ", "T"));
  return isNaN(d.getTime()) ? null : d;
}

/** 相对时间：刚刚 / N 分钟前 / N 小时前 / 昨天 / N 天前 / 日期（用于「最近签到」主信息） */
export function relativeTime(at?: string | null): string {
  const d = parseAt(at);
  if (!d) return "";
  const diff = Date.now() - d.getTime();
  const min = Math.floor(diff / 60000);
  if (min < 1) return "刚刚";
  if (min < 60) return `${min} 分钟前`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr} 小时前`;
  const day = Math.floor(hr / 24);
  if (day === 1) return "昨天";
  if (day < 7) return `${day} 天前`;
  return at!.slice(0, 10);
}

/** 是否为今天（本地时区） */
export function isToday(at?: string | null): boolean {
  const d = parseAt(at);
  if (!d) return false;
  const now = new Date();
  return (
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate()
  );
}

export type SignState = "signing" | "done" | "pending" | "fail" | "inactive";

/** 账号签到状态：用于状态列徽标 + 顶部统计卡片（busy 优先于一切） */
export function signState(a: Account, busy: boolean): SignState {
  if (busy) return "signing";
  const r = a.last;
  if (!r) return "pending";
  if (r.already) return "done";
  if (r.success) return isToday(r.at) ? "done" : "pending";
  if (r.inactive) return "inactive";
  return "fail";
}

/** 一批签到结果的互斥计数（成功 / 已签 / 失败），避免「已签」被重复算成「成功」 */
export function tally(
  items: { success: boolean; already: boolean; inactive: boolean }[]
) {
  return {
    ok: items.filter((l) => l.success && !l.already).length,
    already: items.filter((l) => l.already).length,
    fail: items.filter((l) => !l.success && !l.already && !l.inactive).length,
  };
}

/**
 * 账号最近一次签到结果的徽标。
 *
 * 注意顺序：服务端对「今天已签到」返回 HTTP 400 + `code=10001`，
 * 此时 `success` 也是 true（幂等成功），所以必须**先判 already**，
 * 否则「今日已签」永远显示成「成功」。
 */
/** 徽标只关心这三个互斥字段（CheckinRecord / CheckinLog 都满足） */
type BadgeState = { success: boolean; already: boolean; inactive: boolean } | null;

export function ResultBadge({ last }: { last: BadgeState }) {
  if (!last) return <span className="badge badge-idle">未签到</span>;
  if (last.already) return <span className="badge badge-already">今日已签</span>;
  if (last.success) return <span className="badge badge-ok">成功</span>;
  if (last.inactive) return <span className="badge badge-idle">活动未开</span>;
  return <span className="badge badge-err">失败</span>;
}

export function LogBadge({ log }: { log: CheckinLog }) {
  // 同 ResultBadge：`already` 必须优先于 `success`（已签时二者都为 true）
  if (log.already) return <span className="badge badge-already">今日已签</span>;
  if (log.success) return <span className="badge badge-ok">成功</span>;
  if (log.inactive) return <span className="badge badge-idle">活动未开</span>;
  return <span className="badge badge-err">失败</span>;
}
