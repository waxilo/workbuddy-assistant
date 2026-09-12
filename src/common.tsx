import type { CheckinLog } from "./types";

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

/// 账号在日志/筛选中的展示名：名称 + 手机号（手机号缺失时省略）
export function accountLabel(name: string, phone?: string | null): string {
  return phone ? `${name}（${phone}）` : name;
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
