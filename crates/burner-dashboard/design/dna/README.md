# DNA the DNA design reference

Imported 2026-08-19 from the operator's Claude Design project "DNA SaaS
(Indigo+Orange)" (project 4a3ce4de-d00b-4376-a370-b9f4e6145488), at the
operator's direction: the defraburner dashboard implements THIS design.

Files here are reference material, not served assets:

- gallery-dna.html: the the DNA recolor gallery: the authoritative
  look (void near-black paper, indigo #818cf8 + orange #f97316, Newsreader /
  IBM Plex Sans / JetBrains Mono, cornered edges (radius 0 everywhere),
  panels over shadows, brand gradient indigo->purple->orange).
- tokens.css: base DNA tokens (the gallery's DNA block overrides the
  palette; keep the override semantics).
- app.css: the full component stylesheet (buttons, chips, cards, progress
  family, rings, tables, tabs, banners, kpi/stat, forms, modal/drawer,
  toasts). Reused nearly verbatim minus CDN @imports.
- primitives.jsx: React reference for markup/class contracts. The dashboard
  is vanilla JS: replicate the DOM/class structure, not React.
- data.js: reviewed during import but deliberately NOT stored: it is mock
  data for the design prototypes. The dashboard renders real cluster data
  only (honesty fence); component prop shapes are visible in primitives.jsx.
- dna-dna.css: the extracted DNA override block + gallery chrome from
  gallery-dna.html (the full React demo page itself is not stored; the
  DNA css and primitives.jsx carry everything actionable).

Translation constraints for the served dashboard: no CDN, no React/Babel,
no Google Fonts imports (fonts embedded as woff2), lucide icons inlined as
static SVGs for the icons actually used, dark (void paper) default with the
dna-tinted light theme on the toggle.
