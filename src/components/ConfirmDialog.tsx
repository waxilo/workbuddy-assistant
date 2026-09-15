import { useEffect } from "react";
import type { ConfirmReq } from "../common";
import { Dialog } from "./Dialog";

/**
 * 自研确认框：替代 window.confirm（Tauri 的 WKWebView 不支持原生 confirm 面板）。
 *
 * 遮罩、Esc 关闭、dialog 语义与滚动锁都交给 Dialog —— 这里只额外负责
 * 「Enter 确认」，因为那是确认框独有的语义（普通弹窗按 Enter 不该提交）。
 */
export function ConfirmDialog({
  req,
  onDone,
}: {
  req: ConfirmReq;
  onDone: (ok: boolean) => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Enter") onDone(true);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onDone]);

  return (
    <Dialog
      label={req.title}
      className="confirm"
      maskClassName="confirm-mask"
      onClose={() => onDone(false)}
    >
      <h2>{req.title}</h2>
      {req.body && <p className="confirm-body">{req.body}</p>}
      <div className="modal-actions">
        <button className="btn ghost" autoFocus onClick={() => onDone(false)}>
          取消
        </button>
        <button
          className={req.danger ? "btn danger" : "btn primary"}
          onClick={() => onDone(true)}
        >
          {req.okText ?? "确定"}
        </button>
      </div>
    </Dialog>
  );
}
