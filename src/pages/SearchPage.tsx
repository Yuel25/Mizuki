import { useEffect, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { SkeletonGrid, SubjectGrid } from "../components/SubjectCard";
import type { SearchPayload, Subject } from "../types";

export function SearchPage({ onSelect }: { onSelect: (subject: Subject) => void }) {
  const [query, setQuery] = useState(""),
    [results, setResults] = useState<Subject[]>([]),
    [busy, setBusy] = useState(false),
    [message, setMessage] = useState("输入中文、日文或罗马字搜索番剧");
  useEffect(() => {
    const keyword = query.trim();
    if (keyword.length < 2) {
      setResults([]);
      setBusy(false);
      setMessage("输入至少 2 个字符开始搜索");
      return;
    }
    if (!isTauri()) {
      setResults([]);
      setBusy(false);
      setMessage("搜索需要在 Mizuki 桌面端使用");
      return;
    }
    let active = true;
    setBusy(true);
    const timer = window.setTimeout(async () => {
      try {
        const data = await invoke<SearchPayload>("search_anime", { keyword, limit: 24 });
        if (!active) return;
        setResults(data.subjects);
        setMessage(data.warning || `找到 ${data.subjects.length} 部番剧`);
      } catch (error) {
        if (active) {
          setResults([]);
          setMessage(String(error));
        }
      } finally {
        if (active) setBusy(false);
      }
    }, 350);
    return () => {
      active = false;
      window.clearTimeout(timer);
    };
  }, [query]);
  return (
    <>
      <header>
        <div>
          <p className="eyebrow">BANGUMI SEARCH</p>
          <h1>搜索番剧</h1>
          <p>搜索 Bangumi 动画条目，并直接加入你的追番收藏。</p>
        </div>
      </header>
      <div className="anime-search">
        <span>⌕</span>
        <input
          autoFocus
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="输入番剧名称，例如：葬送的芙莉莲"
        />
        <small>{busy ? "搜索中…" : message}</small>
      </div>
      {busy ? (
        <SkeletonGrid count={4} />
      ) : results.length ? (
        <SubjectGrid subjects={results} onSelect={onSelect} />
      ) : (
        <div className="empty compact">
          <span>⌕</span>
          <h3>暂无搜索结果</h3>
          <p>{message}</p>
        </div>
      )}
    </>
  );
}
