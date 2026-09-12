import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles/app.css";

const root = document.getElementById("root");
if (!root) {
  // 这种情况只可能是 index.html 被改坏了，直接抛出让问题显式化，
  // 而不是静默白屏
  throw new Error("找不到 #root 挂载点");
}

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
