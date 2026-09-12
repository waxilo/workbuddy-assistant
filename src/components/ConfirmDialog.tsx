import { useEffect } from "react";
import type { ConfirmReq } from "../common";

/** 自研确认框：替代 window.confirm（Tauri 的 WKWebView 不支持原生 confirm 面板） */
export function ConfirmDialog({
  req,
  onDone,
}: {
  req: ConfirmReq;
  onDone: (ok: boolean) => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onDone(false);
      if (e.key === "Enter") onDone(true);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onDone]);

  return (
    <div className="modal-mask confirm-mask" onClick={() => onDone(false)}>
      <div className="modal confirm" onClick={(e) => e.stopPropagation()}>
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
      </div>
    </div>
  );
}
