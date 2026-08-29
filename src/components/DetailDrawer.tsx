import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { collectionLabels, loadSubjectDetail } from "../lib";
import type { BangumiComment, Collection, CommentPage, Subject, SubjectDetail } from "../types";

export function DetailDrawer({
  subject,
  close,
  updateCollection,
  setProgress,
  subscribed,
  subscribe,
  unsubscribe,
}: {
  subject: Subject;
  close: () => void;
  updateCollection: (s: Subject, c: Collection) => void;
  setProgress: (s: Subject, w: number) => void;
  subscribed: boolean;
  subscribe: (s: Subject) => void;
  unsubscribe: (s: Subject) => void;
}) {
  const [detail, setDetail] = useState<SubjectDetail | null>(null),
    [comments, setComments] = useState<BangumiComment[]>([]),
    [loading, setLoading] = useState(true),
    [error, setError] = useState(""),
    [commentError, setCommentError] = useState("");
  useEffect(() => {
    let active = true;
    setLoading(true);
    setError("");
    setCommentError("");
    Promise.allSettled([
      loadSubjectDetail(subject.id),
      invoke<CommentPage>("get_comments", { subjectId: subject.id, offset: 0 }),
    ]).then((results) => {
      if (!active) return;
      const [detailResult, commentResult] = results;
      if (detailResult.status === "fulfilled") setDetail(detailResult.value);
      else setError(String(detailResult.reason));
      if (commentResult.status === "fulfilled") setComments(commentResult.value.data || []);
      else setCommentError(String(commentResult.reason));
      setLoading(false);
    });
    return () => {
      active = false;
    };
  }, [subject.id]);
  const episodes = detail?.total_episodes || detail?.eps || subject.episodes;
  const summary = detail?.summary?.trim() || subject.summary?.trim();
  const score = detail?.rating?.score ?? subject.score;
  const rank = detail?.rating?.rank ?? subject.rank;
  const cover = detail?.images?.large || detail?.images?.common || subject.image;
  return (
    <div className="drawer-backdrop" onMouseDown={close}>
      <aside className="detail-drawer" onMouseDown={(e) => e.stopPropagation()}>
        <button className="drawer-close" onClick={close} aria-label="返回番剧列表" title="返回">
          <span>←</span>返回
        </button>
        <div className="detail-hero">
          <div className="detail-cover">{cover ? <img src={cover} /> : subject.nameCn.slice(0, 2)}</div>
          <div>
            <p className="eyebrow">BANGUMI #{subject.id}</p>
            <h2>{subject.nameCn || subject.name}</h2>
            <p>{subject.name}</p>
            <div className="score">
              <strong>{score || "—"}</strong>
              <span>
                Bangumi 评分
                <br />
                排名 #{rank || "—"}
              </span>
            </div>
            <button
              className="ghost bangumi-link"
              onClick={() => openUrl(`https://bgm.tv/subject/${subject.id}`)}
            >
              在 Bangumi 打开 ↗
            </button>
            {subscribed ? (
              <button
                className="ghost bangumi-link"
                title="取消后已有下载任务会保留"
                onClick={() => unsubscribe(subject)}
              >
                ✓ 已订阅新集 · 点击取消
              </button>
            ) : (
              <button
                className="ghost bangumi-link"
                title="新集发布时按规则自动下载"
                onClick={() => subscribe(subject)}
              >
                订阅新集 · 自动下载
              </button>
            )}
          </div>
        </div>
        <div className="collection-select">
          {(Object.keys(collectionLabels) as Collection[]).map((k) => (
            <button
              key={k}
              className={subject.collection === k ? "active" : ""}
              onClick={() => updateCollection(subject, k)}
            >
              {collectionLabels[k]}
            </button>
          ))}
        </div>
        <section>
          <h3>简介</h3>
          {loading && !summary ? (
            <p className="summary muted">正在读取条目详情…</p>
          ) : (
            <p className="summary">{summary || "Bangumi 暂未收录简介"}</p>
          )}
          {error && <small className="detail-error">{error}</small>}
        </section>
        <section>
          <div className="section-title">
            <h3>观看进度</h3>
            <span>
              {subject.watched}/{episodes || "?"}
            </span>
          </div>
          <div className="progress">
            <i style={{ width: `${episodes ? Math.min(100, (subject.watched / episodes) * 100) : 0}%` }} />
          </div>
          <div className="progress-controls">
            <button
              aria-label="减少一集"
              disabled={subject.watched <= 0}
              onClick={() => setProgress(subject, subject.watched - 1)}
            >
              −
            </button>
            <input
              aria-label="观看集数"
              type="number"
              min={0}
              max={episodes || undefined}
              value={subject.watched}
              onChange={(e) => {
                const value = parseInt(e.target.value, 10);
                if (!Number.isNaN(value)) setProgress(subject, value);
              }}
            />
            <button aria-label="看完一集" onClick={() => setProgress(subject, subject.watched + 1)}>
              ＋1
            </button>
            {episodes > 0 && subject.watched < episodes && (
              <button className="mark-finished" onClick={() => setProgress(subject, episodes)}>
                全部看完
              </button>
            )}
          </div>
          <p className="episode-total">共 {episodes || "未知"} 集 · 改动会同步到 Bangumi</p>
        </section>
        <section>
          <div className="section-title">
            <h3>热门短评</h3>
            <span>{comments.length ? `显示 ${comments.length} 条` : "Bangumi 社区"}</span>
          </div>
          {loading && !comments.length ? (
            <div className="comment">
              <p>正在读取短评…</p>
            </div>
          ) : comments.length ? (
            <div className="comment-list">
              {comments.slice(0, 6).map((item) => (
                <article className="comment" key={item.id}>
                  <div className="comment-user">
                    {item.user?.avatar?.small || item.user?.avatar?.medium ? (
                      <img src={item.user.avatar.small || item.user.avatar.medium} />
                    ) : (
                      <span>●</span>
                    )}
                    <b>{item.user?.nickname || item.user?.username || "Bangumi 用户"}</b>
                    {item.rate ? <em>★ {item.rate}</em> : null}
                    {item.spoiler && <i>剧透</i>}
                  </div>
                  <p>
                    {item.spoiler
                      ? "该短评包含剧透，请前往 Bangumi 查看。"
                      : item.comment || "（无文字短评）"}
                  </p>
                </article>
              ))}
            </div>
          ) : commentError ? (
            <div className="comment">
              <b>短评加载失败</b>
              <p>可用上方“在 Bangumi 打开”前往查看原文。</p>
            </div>
          ) : (
            <div className="comment">
              <b>暂无短评</b>
              <p>这个条目目前没有可显示的公开短评。</p>
            </div>
          )}
        </section>
      </aside>
    </div>
  );
}
