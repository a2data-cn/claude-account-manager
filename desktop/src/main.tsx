import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { loadFlagFontIfNeeded } from "./flags";
import "./styles.css";

loadFlagFontIfNeeded();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
