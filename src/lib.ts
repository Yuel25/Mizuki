// 跨页面共享的常量、格式化工具与详情请求去重。

import { invoke } from "@tauri-apps/api/core";
import type { SubjectDetail, View } from "./types";

export const nav: { id: View; label: string; icon: string }[] = [
  { id: "today", label: "今日", icon: "◔" },
  { id: "search", label: "搜索", icon: "⌕" },
  { id: "library", label: "追番", icon: "◇" },
  { id: "rss", label: "RSS", icon: "◒" },
  { id: "downloads", label: "下载", icon: "⇩" },
  { id: "settings", label: "设置", icon: "⚙" },
];

export const weekdays = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];

export const collectionLabels: Record<string, string> = {
  wish: "想看",
  doing: "在看",
  collect: "看过",
  on_hold: "搁置",
  dropped: "抛弃",
};

export const formatSpeed = (v: number) =>
  v >= 1e6
    ? `${(v / 1e6).toFixed(1)} MB/s`
    : v >= 1e3
      ? `${(v / 1e3).toFixed(1)} KB/s`
      : v
        ? `${Math.round(v)} B/s`
        : "0 B/s";

/** Bangumi 周表放送日为 JST（UTC+9）语义：默认 Tab 与后端徽章"今天"保持一致。 */
export const jstToday = () => (new Date(Date.now() + 9 * 3600e3).getUTCDay() + 6) % 7;

const subjectDetailRequests = new Map<number, Promise<SubjectDetail>>();

export function loadSubjectDetail(subjectId: number) {
  const cached = subjectDetailRequests.get(subjectId);
  if (cached) return cached;
  const request = invoke<SubjectDetail>("get_subject_detail", { subjectId }).catch((error) => {
    subjectDetailRequests.delete(subjectId);
    throw error;
  });
  subjectDetailRequests.set(subjectId, request);
  return request;
}

/** 从资源标题截取番剧名（去掉字幕组/集数/画质等尾巴），RSS 分组用。 */
export function releaseAnimeName(title: string) {
  const clean = title.trim().replace(/^(?:\s*\[[^\]]+\]\s*)+/, "");
  const indexes = [
    clean.search(/\s(?:-|–|—)\s*(?:EP?|E)?\d{1,4}(?:v\d+)?(?:\s|\[|$)/i),
    clean.search(/\s第\s*\d+(?:\.\d+)?\s*[话話集]/),
    clean.search(/\s\[(?:\d{3,4}p|web|baha|简|繁|chs|cht|jpn|hevc|avc)/i),
  ].filter((i) => i > 0);
  return (
    (indexes.length ? clean.slice(0, Math.min(...indexes)) : clean).replace(/[\s._-]+$/g, "").trim() ||
    "未识别番剧"
  );
}

export const parseKeywords = (value: string) => value.split(/[,，、\s]+/).filter(Boolean);
