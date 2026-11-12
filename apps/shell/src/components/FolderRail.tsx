import { Inbox, Star, Send, FileText, Archive, Trash2, PenLine } from "lucide-react";
import type { Folder } from "../types";

/**
 * 左侧图标轨（F 版设计的核心）。
 *
 * ## 为什么文件夹从"一栏文字"变成"图标轨"
 *
 * 上一版左栏要放「写邮件按钮 + 账户卡 + 文件夹列表」，11–14rem 宽全被占满，
 * 而姓名/主题这些真正要读的东西挤在中间栏。改成 56px 图标轨后：
 * * 横向空间还给列表与正文；
 * * 文件夹用 Lucide 图标 + 工具提示表达，选中项用左侧竖条 + 主色底标示。
 *
 * ## 图标映射
 *
 * 认得出的用专用图标（收件箱/星标/已发送/草稿/归档/垃圾箱），认不出的
 * （用户自建文件夹）统一用 `FileText`。**不做按名字猜**——猜错比统一更难理解。
 */

/** 按文件夹名（或角色）挑图标。 */
function iconFor(folder: Folder) {
  const name = folder.name.toLowerCase();
  if (name.includes("收件") || name === "inbox") return Inbox;
  if (name.includes("星")) return Star;
  if (name.includes("已发送") || name.includes("sent")) return Send;
  if (name.includes("草稿") || name.includes("draft")) return FileText;
  if (name.includes("归档") || name.includes("archive")) return Archive;
  if (name.includes("垃圾") || name.includes("trash") || name.includes("删除")) return Trash2;
  return FileText;
}

export default function FolderRail({
  folders,
  selectedId,
  onSelect,
  onCompose,
  composeDisabled,
}: {
  folders: Folder[];
  selectedId: string | null;
  onSelect: (folder: Folder) => void;
  onCompose: () => void;
  composeDisabled: boolean;
}) {
  return (
    <>
      <button
        type="button"
        className="compose-launch-button"
        onClick={onCompose}
        disabled={composeDisabled}
        title="写邮件"
        aria-label="写邮件"
      >
        <PenLine size={18} strokeWidth={2} />
      </button>

      <nav className="folder-rail" aria-label="文件夹">
        {folders.map((folder) => {
          const Icon = iconFor(folder);
          const active = folder.id === selectedId;
          return (
            <button
              key={folder.id}
              type="button"
              className={active ? "folder-item folder-item-active" : "folder-item"}
              onClick={() => onSelect(folder)}
              // `title` 是折叠成图标后唯一能读到文件夹名的地方，必须给。
              title={folder.name}
              aria-label={folder.name}
              aria-current={active ? "true" : undefined}
            >
              <Icon size={18} strokeWidth={1.9} />
            </button>
          );
        })}
      </nav>
    </>
  );
}
