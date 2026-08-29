import { useEffect, useState } from "react";
import { loadSubjectDetail } from "../lib";
import type { Subject } from "../types";

export function TodaySection({
  title,
  subtitle,
  count,
  featured = false,
  children,
}: {
  title: string;
  subtitle: string;
  count: number;
  featured?: boolean;
  children: React.ReactNode;
}) {
  return (
    <section className={`today-section${featured ? " featured" : ""}`}>
      <div className="today-section-title">
        <div>
          <h2>{title}</h2>
          <p>{subtitle}</p>
        </div>
        <span>{count}</span>
      </div>
      {children}
    </section>
  );
}

export function SubjectGrid({ subjects, onSelect }: { subjects: Subject[]; onSelect: (s: Subject) => void }) {
  if (!subjects.length)
    return (
      <div className="empty">
        <span>☾</span>
        <h3>这里暂时没有番剧</h3>
        <p>刷新数据或切换其他分类看看。</p>
      </div>
    );
  return (
    <section className="subject-grid">
      {subjects.map((s) => (
        <SubjectCard key={s.id} subject={s} onSelect={onSelect} />
      ))}
    </section>
  );
}

export function SkeletonGrid({ count = 6 }: { count?: number }) {
  return (
    <section className="subject-grid" aria-busy="true">
      {Array.from({ length: count }, (_, i) => (
        <div className="skeleton-card" key={i}>
          <div className="skeleton cover-sk" />
          <div className="card-content">
            <div className="skeleton line-sk" />
            <div className="skeleton line-sk short" />
          </div>
        </div>
      ))}
    </section>
  );
}

function SubjectCard({ subject: s, onSelect }: { subject: Subject; onSelect: (s: Subject) => void }) {
  const [resolved, setResolved] = useState({
    episodes: s.episodes,
    score: s.score,
    rank: s.rank,
    image: s.image,
  });
  useEffect(() => {
    setResolved({ episodes: s.episodes, score: s.score, rank: s.rank, image: s.image });
    if (s.score > 0 && s.episodes > 0) return;
    let active = true;
    loadSubjectDetail(s.id)
      .then((value) => {
        if (active)
          setResolved((current) => ({
            episodes: value.total_episodes ?? value.eps ?? current.episodes,
            score: value.rating?.score ?? current.score,
            rank: value.rating?.rank ?? current.rank,
            image: value.images?.large ?? value.images?.common ?? current.image,
          }));
      })
      .catch(() => {});
    return () => {
      active = false;
    };
  }, [s.id, s.episodes, s.score, s.rank, s.image]);
  const hydrated = { ...s, ...resolved };
  return (
    <article className="subject-card" onClick={() => onSelect(hydrated)}>
      <div className="cover">
        {resolved.image ? (
          <img src={resolved.image} alt="" />
        ) : (
          <span>{(s.nameCn || s.name).slice(0, 2)}</span>
        )}
        <b className={`state ${s.updateState}`}>
          {s.updateState === "published"
            ? "有更新"
            : s.updateState === "downloading"
              ? "下载中"
              : s.updateState === "completed"
                ? "已下载"
                : ""}
        </b>
      </div>
      <div className="card-content">
        <h3>{s.nameCn || s.name}</h3>
        <p>{s.name}</p>
        <div className="meta">
          <strong>★ {resolved.score > 0 ? resolved.score.toFixed(1) : "—"}</strong>
          {resolved.rank && <span>#{resolved.rank}</span>}
          <span>
            {s.watched}/{resolved.episodes || "?"} 话
          </span>
        </div>
      </div>
    </article>
  );
}
