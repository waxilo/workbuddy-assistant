import { check, type DownloadEvent } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

export interface UpdateProgress {
  status:
    | "checking"
    | "available"
    | "downloading"
    | "installing"
    | "updated"
    | "no-update"
    | "error";
  message: string;
  /** 已下载字节数（仅 downloading 时有意义） */
  downloaded?: number;
  /** 总字节数；undefined / 0 = 服务端没给 Content-Length，进度不可知 */
  total?: number;
}

/** 可直接渲染的下载进度；percent 为 null 表示「总量未知」，UI 走不确定态 */
export interface DownloadProgress {
  downloaded: number;
  total: number;
  percent: number | null;
}

/**
 * 把 UpdateProgress 收敛成能直接渲染的进度；非下载中返回 null（不显示进度条）。
 *
 * 独立出来是因为「百分比怎么算」属于更新逻辑，不该散在页面里；
 * 页面只消费 { downloaded, total, percent } 三个数。
 */
export function downloadProgress(p: UpdateProgress | null): DownloadProgress | null {
  if (!p || p.status !== "downloading") return null;
  const downloaded = p.downloaded ?? 0;
  const total = p.total ?? 0;
  return {
    downloaded,
    total,
    percent: total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : null,
  };
}

/**
 * 检查并安装 GitHub Release 上的更新。
 * onProgress 用于驱动 UI 进度展示；安装完成后自动重启应用。
 */
export async function checkAndInstall(
  onProgress: (p: UpdateProgress) => void
): Promise<void> {
  onProgress({ status: "checking", message: "正在检查更新…" });
  let update;
  try {
    update = await check();
  } catch (e) {
    onProgress({
      status: "error",
      message: "检查更新失败：" + errMsg(e),
    });
    return;
  }

  if (!update) {
    onProgress({ status: "no-update", message: "已经是最新版本。" });
    return;
  }

  onProgress({
    status: "available",
    message: `发现新版本 ${update.version}`,
  });

  try {
    let downloaded = 0;
    // 总大小只在 Started 事件里给（Progress 事件只有 chunkLength），必须在这里记住它；
    // 若去 Progress 里取 contentLength，total 恒为 undefined → 百分比恒为 0 → 进度条永远是空条。
    let total = 0;
    const emit = () =>
      onProgress({
        status: "downloading",
        message: "正在下载更新…",
        downloaded,
        total,
      });
    emit();

    // switch 直接收窄 data 类型 —— 不要用 as 断言，
    // 正是「把 Progress 的数据断言成含 contentLength」才让这个 bug 躲过了 tsc。
    const onEvent = (event: DownloadEvent) => {
      switch (event.event) {
        case "Started":
          total = event.data.contentLength ?? 0;
          emit();
          break;
        case "Progress":
          downloaded += event.data.chunkLength;
          emit();
          break;
        case "Finished":
          onProgress({ status: "installing", message: "正在安装更新…" });
          break;
      }
    };
    await update.downloadAndInstall(onEvent);
  } catch (e) {
    onProgress({ status: "error", message: "更新失败：" + errMsg(e) });
    return;
  }

  onProgress({ status: "updated", message: "更新完成，正在重启…" });
  try {
    await relaunch();
  } catch (e) {
    onProgress({
      status: "error",
      message: "安装完成但重启失败，请手动重启：" + errMsg(e),
    });
  }
}

function errMsg(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}
