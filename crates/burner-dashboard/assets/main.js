// defraburner dashboard -- boot glue. Every view module wires its own
// forms and `Burner.onOverview`/`onDecision`/`onCellChange` handlers at
// load time (see core.js's own `DOMContentLoaded` for theme/token-gate/
// sidebar init); this file's only remaining job is the Console section's
// Data/Raw-GraphQL tab switch, which belongs to no single view module.
"use strict";

(function () {
  const B = window.Burner;

  function initConsoleTabs() {
    const tabs = B.$all(".tab[data-console-tab]");
    if (tabs.length === 0) return;
    tabs.forEach((tab) => {
      tab.addEventListener("click", () => {
        tabs.forEach((t) => t.classList.remove("active"));
        tab.classList.add("active");
        B.$all("[data-console-panel]").forEach((panel) => {
          panel.hidden = panel.dataset.consolePanel !== tab.dataset.consoleTab;
        });
      });
    });
  }

  document.addEventListener("DOMContentLoaded", initConsoleTabs);
})();
