import { useEffect, type ReactNode } from "react";

/**
 * 遮罩层 + 弹窗外壳。全应用所有弹窗都必须走它。
 *
 * 抽出来的直接原因：弹窗在 3 个文件里出现了 5 次，每次都手写
 * `.modal-mask` + 点遮罩关闭 + `stopPropagation`。而只有「确认框」实现了
 * Esc 关闭，**另外 4 处按 Esc 关不掉**；5 处都没有 dialog 语义
 * （role / aria-modal），也没有背景滚动锁。行为散落在各使用点，必然漏掉几个。
 *
 * 现在这些行为只在这里实现一次：任何新弹窗自动获得 Esc、遮罩关闭、
 * 无障碍语义与滚动锁。
 */
export function Dialog({
  onClose,
  className,
  maskClassName,
  label,
  children,
}: {
  /** 关闭（点遮罩 / 按 Esc） */
  onClose: () => void;
  /** 附加类名（作用在弹窗本体），如 "wide" / "confirm" */
  className?: string;
  /** 附加类名（作用在遮罩层），用于层级覆盖，如 "confirm-mask" */
  maskClassName?: string;
  /** 无障碍名称：通常传弹窗标题文字 */
  label?: string;
  children: ReactNode;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    // 背景滚动锁：本应用真正滚动的是内容区，不是 body，所以要给 body 挂个
    // 状态类让内容区停止滚动（见 ui.css 的 body.modal-open）。
    document.body.classList.add("modal-open");
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.classList.remove("modal-open");
    };
  }, [onClose]);

  return (
    <div
      className={"modal-mask" + (maskClassName ? " " + maskClassName : "")}
      onClick={onClose}
      role="presentation"
    >
      <div
        className={"modal" + (className ? " " + className : "")}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        onClick={(e) => e.stopPropagation()}
      >
        {children}
      </div>
    </div>
  );
}
