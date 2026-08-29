import { useEffect, useState } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { Theme } from "../types";

export function WindowControls() {
  const [theme, setTheme] = useState<Theme>(() =>
    localStorage.getItem("mizuki-theme") === "light" ? "light" : "dark",
  );
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.style.colorScheme = theme;
    localStorage.setItem("mizuki-theme", theme);
  }, [theme]);
  if (!isTauri()) return null;
  const window = getCurrentWindow();
  return (
    <div className="floating-window-controls">
      <button
        className="theme-toggle"
        aria-label="切换深色或浅色模式"
        title={theme === "dark" ? "切换到浅色模式" : "切换到深色模式"}
        onClick={() => setTheme((value) => (value === "dark" ? "light" : "dark"))}
      >
        {theme === "dark" ? "☀" : "☾"}
      </button>
      <button aria-label="最小化" title="最小化" onClick={() => window.minimize()}>
        <i className="minimize-icon" />
      </button>
      <button aria-label="最大化或还原" title="最大化或还原" onClick={() => window.toggleMaximize()}>
        <i className="maximize-icon" />
      </button>
      <button className="close-window" aria-label="关闭" title="关闭到托盘" onClick={() => window.close()}>
        <i className="close-icon" />
      </button>
    </div>
  );
}
