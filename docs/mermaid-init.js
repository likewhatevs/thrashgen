// Render the ```mermaid blocks with Mermaid from a CDN: mdBook has no
// diagram support of its own, and GitHub renders the same blocks in
// the repository's view.
(function () {
  const blocks = document.querySelectorAll("pre > code.language-mermaid");
  if (blocks.length === 0) {
    return;
  }
  const script = document.createElement("script");
  script.src = "https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.min.js";
  script.onload = function () {
    for (const code of blocks) {
      const pre = document.createElement("pre");
      pre.className = "mermaid";
      pre.textContent = code.textContent;
      code.parentElement.replaceWith(pre);
    }
    const dark = !document.documentElement.classList.contains("light");
    mermaid.initialize({ startOnLoad: false, theme: dark ? "dark" : "default" });
    mermaid.run({ nodes: document.querySelectorAll("pre.mermaid") });
  };
  document.head.appendChild(script);
})();
