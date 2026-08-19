/* global React */
// DNA design system - shared primitives library (REFERENCE COPY, verbatim import).
// defraburner's dashboard is vanilla JS: replicate each component's DOM/class
// structure and port the pure helpers (clampPct, ringGeom, stackedSegments...)
// exactly; React itself is NOT used in served assets.

// ===== Icon (Lucide wrapper) =================================
const Icon = ({ name, size = 16, color, style, className = "", ...rest }) => {
  const ref = React.useRef(null);
  React.useEffect(() => {
    if (window.lucide && ref.current) {
      ref.current.innerHTML = "";
      const i = document.createElement("i");
      i.setAttribute("data-lucide", name);
      ref.current.appendChild(i);
      window.lucide.createIcons({ attrs: { "stroke-width": 1.75 } });
    }
  }, [name]);
  return (
    <span
      ref={ref}
      className={className}
      style={{
        display: "inline-flex",
        alignItems: "center",
        justifyContent: "center",
        width: size,
        height: size,
        color,
        flexShrink: 0,
        ...style,
      }}
      aria-hidden="true"
      {...rest}
    />
  );
};

// ===== Button ===============================================
const Button = ({ variant = "secondary", size, icon, iconAfter, loading, children, block, as: As, href, ...rest }) => {
  const cls = [
    "btn",
    `btn-${variant}`,
    size === "sm" && "btn-sm",
    size === "lg" && "btn-lg",
    block && "btn-block",
    !children && icon && "btn-icon",
    rest.className,
  ].filter(Boolean).join(" ");
  const content = (
    <>
      {loading ? <span className="spinner" /> : icon ? <Icon name={icon} size={size === "sm" ? 14 : 16} /> : null}
      {children}
      {iconAfter ? <Icon name={iconAfter} size={size === "sm" ? 14 : 16} /> : null}
    </>
  );
  if (href || As === "a") {
    return <a className={cls} href={href} {...rest}>{content}</a>;
  }
  const Tag = As || "button";
  return <Tag className={cls} {...rest}>{content}</Tag>;
};

const ButtonGroup = ({ value, onChange, options }) => (
  <div className="btn-group" role="tablist">
    {options.map((o) => (
      <button key={o.value} role="tab" aria-selected={value === o.value}
        className={`btn ${value === o.value ? "active" : ""}`} onClick={() => onChange(o.value)}>
        {o.icon ? <Icon name={o.icon} size={14} /> : null}
        {o.label}
      </button>
    ))}
  </div>
);

// ===== Chip / Dot / KBD =====================================
const CHIP_KIND_ALIAS = { ok: "success", crit: "error", warn: "warning", info: "accent", danger: "error" };
const chipKind = (kind) => CHIP_KIND_ALIAS[kind] || kind;
const Chip = ({ kind = "neutral", pulse, mono, dot = true, size, children, className = "", style, ...rest }) => (
  <span className={`chip chip-${chipKind(kind)} ${mono ? "chip-mono" : ""} ${size === "sm" ? "chip-sm" : ""} ${className}`} style={style} {...rest}>
    {dot ? <span className={`d ${pulse ? "pulse-dot" : ""}`} /> : null}
    {children}
  </span>
);
const Dot = ({ kind = "neutral" }) => <span className={`dot dot-${chipKind(kind)}`} />;
const Kbd = ({ children }) => <span className="kbd">{children}</span>;

// ===== Form fields ==========================================
const Field = ({ label, help, error, children, optional }) => (
  <div className={`field ${error ? "invalid" : ""}`}>
    {label ? (
      <label>
        {label}
        {optional ? <span style={{ color: "var(--fg-3)", fontWeight: 400, marginLeft: 4 }}>optional</span> : null}
      </label>
    ) : null}
    {children}
    {error ? <span className="err">{error}</span> : help ? <span className="help">{help}</span> : null}
  </div>
);

const Input = React.forwardRef(({ mono, prefix, suffix, ...rest }, ref) => {
  const cls = `input ${mono ? "input-mono" : ""} ${rest.className || ""}`;
  if (prefix || suffix) {
    return (
      <span className="input-group">
        {prefix ? <span className="add">{prefix}</span> : null}
        <input ref={ref} {...rest} className={cls} />
        {suffix ? <span className="add">{suffix}</span> : null}
      </span>
    );
  }
  return <input ref={ref} {...rest} className={cls} />;
});

const Select = ({ children, ...rest }) => (
  <select className="select" {...rest}>{children}</select>
);

const Textarea = (props) => <textarea className="textarea" {...props} />;

const Checkbox = ({ label, checked, onChange, name }) => (
  <label className="checkbox">
    <input type="checkbox" checked={checked} onChange={onChange} name={name} />
    <span>{label}</span>
  </label>
);

const Radio = ({ label, checked, onChange, name, value }) => (
  <label className="radio">
    <input type="radio" checked={checked} onChange={onChange} name={name} value={value} />
    <span>{label}</span>
  </label>
);

const Toggle = ({ on, onChange }) => (
  <button type="button" role="switch" aria-checked={!!on}
    className={`toggle ${on ? "on" : ""}`} onClick={() => onChange(!on)} />
);

// ===== Sparkline ============================================
const Sparkline = ({ data, color = "var(--dna-saffron-500)", height = 32, replayIndex = -1, breachAt = -1 }) => {
  const W = 200, H = height;
  if (!data || !data.length) return null;
  const min = Math.min(...data), max = Math.max(...data);
  const range = (max - min) || 1;
  const stepX = W / (data.length - 1);
  const pts = data.map((d, i) => [i * stepX, H - ((d - min) / range) * (H - 6) - 3]);
  const path = pts.map((p, i) => `${i ? "L" : "M"}${p[0].toFixed(1)},${p[1].toFixed(1)}`).join(" ");
  const fillPath = path + ` L${W},${H} L0,${H} Z`;
  const last = pts[pts.length - 1];
  return (
    <svg className="sparkline" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none">
      <defs>
        <linearGradient id={`spark-fill-${color.replace(/[^a-z]/gi, "")}`} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={color} stopOpacity="0.18" />
          <stop offset="100%" stopColor={color} stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={fillPath} fill={`url(#spark-fill-${color.replace(/[^a-z]/gi, "")})`} />
      <path d={path} fill="none" stroke={color} strokeWidth="1.5" strokeLinejoin="round" strokeLinecap="round" />
      {replayIndex >= 0 && pts[replayIndex] ? (
        <g>
          <line x1={pts[replayIndex][0]} x2={pts[replayIndex][0]} y1="0" y2={H} stroke="#22D3EE" strokeWidth="1" strokeDasharray="2 2" opacity="0.6" />
          <circle cx={pts[replayIndex][0]} cy={pts[replayIndex][1]} r="2.5" fill="#22D3EE" />
        </g>
      ) : null}
      {breachAt >= 0 && pts[breachAt] ? (
        <circle cx={pts[breachAt][0]} cy={pts[breachAt][1]} r="3" fill="var(--dna-crimson-witness)" />
      ) : null}
      <circle cx={last[0]} cy={last[1]} r="2" fill={color} />
    </svg>
  );
};

// ===== Toast ================================================
let __toastSetter = null;
const ToastHost = () => {
  const [toasts, setToasts] = React.useState([]);
  React.useEffect(() => { __toastSetter = setToasts; return () => { __toastSetter = null; }; }, []);
  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast ${t.kind || ""}`}>
          <div style={{ display: "flex", alignItems: "flex-start", gap: 8 }}>
            {t.icon ? <Icon name={t.icon} size={14} /> : null}
            <div>
              <div style={{ fontWeight: 600 }}>{t.title}</div>
              {t.body ? <div style={{ color: "var(--fg-3)", marginTop: 2 }}>{t.body}</div> : null}
            </div>
          </div>
        </div>
      ))}
    </div>
  );
};
const toast = (t) => {
  if (!__toastSetter) return;
  const id = Math.random().toString(36).slice(2);
  __toastSetter((arr) => [...arr, { id, ...t }]);
  setTimeout(() => __toastSetter && __toastSetter((arr) => arr.filter((x) => x.id !== id)), t.duration || 4200);
};

// ===== Modal / Drawer =======================================
const Modal = ({ open, onClose, title, sub, footer, children, size = "md" }) => {
  React.useEffect(() => {
    if (!open) return;
    const k = (e) => { if (e.key === "Escape") onClose && onClose(); };
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <>
      <div className="scrim" onClick={onClose} />
      <div className="modal" style={{ width: size === "lg" ? 720 : size === "sm" ? 420 : 520 }} role="dialog" aria-modal="true">
        {title ? (
          <div className="modal-h">
            <h2>{title}</h2>
            {sub ? <div className="sub">{sub}</div> : null}
          </div>
        ) : null}
        <div className="modal-b">{children}</div>
        {footer ? <div className="modal-f">{footer}</div> : null}
      </div>
    </>
  );
};

const Drawer = ({ open, onClose, title, sub, actions, children, width = 920 }) => {
  React.useEffect(() => {
    if (!open) return;
    const k = (e) => { if (e.key === "Escape") onClose && onClose(); };
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <>
      <div className="scrim" onClick={onClose} />
      <div className="drawer" style={{ width }}>
        <div className="drawer-h">
          <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
            <h2>{title}</h2>
            {sub ? <div style={{ color: "var(--fg-3)", fontSize: 12, fontFamily: "var(--font-mono)" }}>{sub}</div> : null}
          </div>
          <div className="spacer" />
          {actions}
          <Button variant="ghost" icon="x" onClick={onClose} aria-label="Close" />
        </div>
        <div className="drawer-b">{children}</div>
      </div>
    </>
  );
};

// ===== Tabs =================================================
const Tabs = ({ value, onChange, options }) => (
  <div className="tabs" role="tablist">
    {options.map((o) => (
      <button key={o.value} role="tab" aria-selected={value === o.value}
        className={`tab ${value === o.value ? "active" : ""}`} onClick={() => onChange(o.value)}>
        {o.icon ? <Icon name={o.icon} size={14} /> : null}
        {o.label}
        {o.count != null ? <span className="count">{o.count}</span> : null}
      </button>
    ))}
  </div>
);

// ===== Banner ===============================================
const Banner = ({ kind = "info", icon, title, children, action }) => (
  <div className={`banner ${kind}`}>
    {icon ? <Icon name={icon} size={16} /> : null}
    <div style={{ flex: 1 }}>
      {title ? <div style={{ fontWeight: 600, marginBottom: 2 }}>{title}</div> : null}
      <div style={{ color: "var(--fg-2)" }}>{children}</div>
    </div>
    {action}
  </div>
);

// ===== KPI card =============================================
const Kpi = ({ label, value, unit, accent, delta, deltaKind = "neutral" }) => (
  <div className="kpi">
    <span className="lbl">{label}</span>
    <span className={`v ${accent || ""}`}>{value}{unit ? <span style={{ fontFamily: "var(--font-sans)", fontSize: 13, color: "var(--fg-3)", fontWeight: 500, marginLeft: 4 }}>{unit}</span> : null}</span>
    {delta ? <span className={`delta ${deltaKind}`}>{delta}</span> : null}
  </div>
);

// ===== Steps indicator ======================================
const Steps = ({ steps, current }) => (
  <div className="steps">
    {steps.map((s, i) => (
      <React.Fragment key={i}>
        <div className={`s ${i < current ? "done" : i === current ? "current" : ""}`}>
          <span className="n">{i < current ? <Icon name="check" size={12} /> : i + 1}</span>
          <span className="lbl">{s}</span>
        </div>
        {i < steps.length - 1 ? <span className="bar" /> : null}
      </React.Fragment>
    ))}
  </div>
);

// ===== Empty state ==========================================
const EmptyState = ({ icon, title, children, action }) => (
  <div className="empty-state">
    {icon ? <Icon name={icon} size={32} className="ico-32" /> : null}
    {title ? <h3>{title}</h3> : null}
    {children ? <p>{children}</p> : null}
    {action ? <div style={{ marginTop: 14 }}>{action}</div> : null}
  </div>
);

// ===== Code block ===========================================
const Code = ({ lang, children }) => (
  <div className="code-wrap">
    {lang ? <div className="code-tabs"><div className="code-tab active">{lang}</div></div> : null}
    <pre className="code"><code>{children}</code></pre>
  </div>
);

// ===== Progress helpers (pure; port these exactly) ==========
const clampPct = (value) => {
  const n = typeof value === "number" ? value : Number(value);
  if (!Number.isFinite(n)) return n === Infinity ? 100 : 0;
  if (n < 0) return 0;
  if (n > 100) return 100;
  return n;
};
const pctLabel = (value, opts) => {
  if (value == null) return null;
  return `${clampPct(value).toFixed((opts && opts.digits) ?? 0)}%`;
};
const progressFillClass = (opts = {}) => {
  const cls = ["progress-fill", opts.variant ?? "accent"];
  if (opts.indeterminate) { cls.push("indeterminate"); return cls.join(" "); }
  if (opts.striped || opts.animateStripes) cls.push("striped");
  if (opts.animateStripes) cls.push("animate");
  if (opts.scan) cls.push("scan");
  return cls.join(" ");
};
const ringGeom = (size, thickness) => {
  const r = (size - thickness) / 2;
  return { r, cx: size / 2, cy: size / 2, circumference: 2 * Math.PI * r };
};
const ringDashOffset = (value, r) => {
  const c = 2 * Math.PI * r;
  return value == null ? c : c * (1 - clampPct(value) / 100);
};
const ringConicGradient = (value, color, track) => {
  if (value == null) return `conic-gradient(${track} 0deg, ${track} 360deg)`;
  return `conic-gradient(${color} ${clampPct(value) * 3.6}deg, ${track} 0)`;
};
const segmentsFilled = (done, total) => {
  const t = Number.isFinite(total) && total > 0 ? Math.floor(total) : 0;
  const d = Number.isFinite(done) ? Math.floor(done) : 0;
  if (d < 0) return 0;
  return d > t ? t : d;
};
const variantColorVar = (variant) => {
  switch (variant) {
    case "replay":  return "var(--dna-cyan-replay)";
    case "success": return "var(--dna-success)";
    case "warning": return "var(--dna-warning)";
    case "witness": return "var(--dna-crimson-witness)";
    case "ink":     return "var(--dna-ink-800)";
    case "accent":
    default:        return "var(--dna-saffron-500)";
  }
};
const STACK_PALETTE = [
  "var(--dna-saffron-500)", "var(--dna-cyan-replay)",
  "var(--dna-success)", "var(--dna-crimson-witness)", "var(--dna-ink-700)",
];
const stackedSegments = (parts, total) => {
  const list = parts ?? [];
  const vals = list.map((p) => (Number.isFinite(p.value) && p.value > 0 ? p.value : 0));
  const sum = vals.reduce((a, b) => a + b, 0);
  const denom = Number.isFinite(total) && total > 0 ? total : sum;
  return list.map((p, i) => ({
    label: p.label,
    color: p.color ?? (p.variant ? variantColorVar(p.variant) : STACK_PALETTE[i % STACK_PALETTE.length]),
    value: vals[i],
    pct: denom > 0 ? (vals[i] / denom) * 100 : 0,
  }));
};

// ===== Progress / SegmentedProgress / StackedProgress / Ring =====
const Progress = ({ value = null, variant = "accent", size = "md", striped, animateStripes, scan, buffer = null, label, showValue = "auto", className = "", ...rest }) => {
  const indeterminate = value === null || value === undefined;
  const pct = indeterminate ? null : clampPct(value);
  const sizeClass = size && size !== "md" ? ` ${size}` : "";
  const fillClass = progressFillClass({ variant, striped, animateStripes, scan, indeterminate });
  const showInlinePct = !indeterminate && showValue !== false && size === "xl";
  const showHeaderPct = !indeterminate && showValue === true && size !== "xl";
  return (
    <div className={`progress${className ? ` ${className}` : ""}`} {...rest}>
      {(label != null || showHeaderPct) && (
        <div className="progress-label">
          {label != null && <span className="name">{label}</span>}
          {showHeaderPct && <span className={`pct ${variant}`}>{pctLabel(pct)}</span>}
        </div>
      )}
      <div className={`progress-track${sizeClass}`} role="progressbar" aria-busy={indeterminate || undefined} aria-valuemin={0} aria-valuemax={100} aria-valuenow={indeterminate ? undefined : Math.round(pct)} aria-label={typeof label === "string" ? label : undefined}>
        {!indeterminate && buffer != null && <span className="progress-buffer" style={{ width: `${clampPct(buffer)}%` }} />}
        <div className={fillClass} style={indeterminate ? undefined : { width: `${pct}%` }}>
          {showInlinePct && <span className="inline-pct">{pctLabel(pct)}</span>}
        </div>
      </div>
    </div>
  );
};

const SegmentedProgress = ({ total = 0, done = 0, variant = "accent", size = "md", label, showCount = true, className = "", ...rest }) => {
  const t = Number.isFinite(total) && total > 0 ? Math.floor(total) : 0;
  const filled = segmentsFilled(done, t);
  const tall = size === "lg" || size === "xl";
  return (
    <div className={`progress${className ? ` ${className}` : ""}`} {...rest}>
      {(label != null || showCount) && (
        <div className="progress-label">
          {label != null && <span className="name">{label}</span>}
          {showCount && <span className={`pct ${variant}`}>{filled}/{t}</span>}
        </div>
      )}
      <div className={`progress-seg${tall ? " tall" : ""}`} role="progressbar" aria-valuemin={0} aria-valuemax={t} aria-valuenow={filled} aria-valuetext={`${filled} of ${t}`} aria-label={typeof label === "string" ? label : undefined}>
        {Array.from({ length: t }, (_, i) => <span key={i} className={`tick${i < filled ? ` on ${variant}` : ""}`} />)}
      </div>
    </div>
  );
};

const StackedProgress = ({ parts = [], total, label, legend = true, className = "", ...rest }) => {
  const segs = stackedSegments(parts, total);
  return (
    <div className={`progress${className ? ` ${className}` : ""}`} {...rest}>
      {label != null && <div className="progress-label"><span className="name">{label}</span></div>}
      <div className="progress-stack" role="img" aria-label={typeof label === "string" ? label : "breakdown"}>
        {segs.map((s, i) => <span key={i} style={{ width: `${s.pct}%`, background: s.color }} title={`${s.label}: ${pctLabel(s.pct)}`} />)}
      </div>
      {legend && segs.length > 0 && (
        <div className="progress-stack-legend">
          {segs.map((s, i) => (
            <span key={i} className="item">
              <span className="sw" style={{ background: s.color, color: s.color }} />
              {s.label} <b>{pctLabel(s.pct)}</b>
            </span>
          ))}
        </div>
      )}
    </div>
  );
};

const Ring = ({ value = null, variant = "accent", size = 72, thickness = 8, render = "svg", label, showValue = true, className = "", ...rest }) => {
  const indeterminate = value === null || value === undefined;
  const pct = indeterminate ? null : clampPct(value);
  const { r, cx, cy, circumference } = ringGeom(size, thickness);
  const color = variantColorVar(variant);
  const centre = label != null ? label : showValue && !indeterminate ? pctLabel(pct) : null;
  const wrapProps = {
    className: `progress-ring-wrap progress-ring ${variant}${className ? ` ${className}` : ""}`,
    style: { width: size, height: size },
    role: "progressbar", "aria-busy": indeterminate || undefined,
    "aria-valuemin": 0, "aria-valuemax": 100,
    "aria-valuenow": indeterminate ? undefined : Math.round(pct),
    "aria-label": typeof label === "string" ? label : undefined, ...rest,
  };
  if (render === "conic") {
    return (
      <div {...wrapProps}>
        <div className={`ring-conic${indeterminate ? " is-spin" : ""}`} style={{ width: size, height: size, background: ringConicGradient(value, color, "var(--bg-sunken)"), "--ring-thickness": `${thickness}px` }} />
        {centre != null && <div className="ring-label">{centre}</div>}
      </div>
    );
  }
  return (
    <div {...wrapProps}>
      <svg className={`ring-svg ${variant}${indeterminate ? " is-spin" : ""}`} width={size} height={size} viewBox={`0 0 ${size} ${size}`}>
        <circle className="track" cx={cx} cy={cy} r={r} fill="none" strokeWidth={thickness} />
        <circle className="meter" cx={cx} cy={cy} r={r} fill="none" strokeWidth={thickness} stroke={color} strokeDasharray={circumference} strokeDashoffset={indeterminate ? circumference * 0.75 : ringDashOffset(value, r)} />
      </svg>
      {centre != null && <div className="ring-label">{centre}</div>}
    </div>
  );
};

// ===== DataTable ============================================
const DataTable = ({ columns = [], rows, rowKey, searchable = true, searchPlaceholder = "Search…", empty = "No results.", toolbar, expand, maxHeight, className = "", ...rest }) => {
  const [q, setQ] = React.useState("");
  const [open, setOpen] = React.useState(() => new Set());
  const query = q.trim().toLowerCase();
  const hasExpand = typeof expand === "function";
  const toggle = (k) => setOpen((s) => { const n = new Set(s); n.has(k) ? n.delete(k) : n.add(k); return n; });
  const searchCols = columns.filter((c) => typeof c.search === "function");
  const list = rows ?? [];
  const filtered = !query || searchCols.length === 0 ? list
    : list.filter((r) => searchCols.some((c) => String(c.search(r) ?? "").toLowerCase().includes(query)));
  return (
    <div className={`data-table-wrap ${className}`} {...rest}>
      {(searchable && searchCols.length > 0) || toolbar ? (
        <div className="data-table-toolbar">
          {searchable && searchCols.length > 0
            ? <Input prefix={<Icon name="search" size={14} />} value={q} onChange={(e) => setQ(e.target.value)} placeholder={searchPlaceholder} aria-label="Search" className="data-table-search" />
            : <span />}
          <span className="data-table-count">{filtered.length}{rows ? ` of ${list.length}` : ""}</span>
          {toolbar}
        </div>
      ) : null}
      <div className="scroll-area" style={maxHeight ? { maxHeight } : undefined}>
        <table className="ui-data-table">
          <thead>
            <tr>
              {hasExpand ? <th className="ui-data-table-expander" /> : null}
              {columns.map((c, i) => <th key={c.key ?? i} style={{ textAlign: c.align || "left", width: c.width }}>{c.header}</th>)}
            </tr>
          </thead>
          <tbody>
            {filtered.length === 0 ? (
              <tr><td colSpan={columns.length + (hasExpand ? 1 : 0)} className="ui-data-table-empty">{empty}</td></tr>
            ) : filtered.map((r, i) => {
              const k = rowKey ? rowKey(r) : i;
              const detail = hasExpand ? expand(r) : null;
              const isOpen = open.has(k);
              return (
                <React.Fragment key={k}>
                  <tr>
                    {hasExpand ? (
                      <td className="ui-data-table-expander">
                        {detail ? <button type="button" className="ui-data-table-toggle" aria-expanded={isOpen} aria-label={isOpen ? "Collapse" : "Expand"} onClick={() => toggle(k)}><Icon name={isOpen ? "chevron-down" : "chevron-right"} size={14} /></button> : null}
                      </td>
                    ) : null}
                    {columns.map((c, ci) => <td key={c.key ?? ci} style={{ textAlign: c.align || "left" }}>{c.cell ? c.cell(r) : r[c.key]}</td>)}
                  </tr>
                  {isOpen && detail ? <tr className="ui-data-table-detail"><td colSpan={columns.length + 1}>{detail}</td></tr> : null}
                </React.Fragment>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
};

// ===== Card / SectionLabel / PageHeader / Stat / Toolbar =====
const Card = ({ title, subtitle, actions, href, icon, tone = "raised", pad = true, interactive, className = "", children, ...rest }) => {
  const hasHead = title != null || actions != null || icon != null;
  const inner = (
    <>
      {hasHead ? (
        <div className="ui-card-head">
          {icon ? <Icon name={icon} size={16} /> : null}
          <div className="ui-card-titles">
            {title != null ? <span className="ui-card-title">{title}</span> : null}
            {subtitle != null ? <span className="ui-card-sub">{subtitle}</span> : null}
          </div>
          {actions != null ? <div className="ui-card-actions">{actions}</div> : null}
        </div>
      ) : null}
      <div className={pad ? "ui-card-body" : ""}>{children}</div>
    </>
  );
  const cls = `ui-card tone-${tone}${interactive || href ? " is-interactive" : ""} ${className}`;
  return href ? <a href={href} className={cls} {...rest}>{inner}</a> : <div className={cls} {...rest}>{inner}</div>;
};

const SectionLabel = ({ children, className = "", ...rest }) => (
  <div className={`ui-section-label ${className}`} {...rest}>{children}</div>
);

const PageHeader = ({ title, subtitle, actions, className = "", ...rest }) => (
  <div className={`ui-page-header ${className}`} {...rest}>
    <div className="ui-page-header-titles">
      <h1>{title}</h1>
      {subtitle != null ? <p className="ui-page-header-sub">{subtitle}</p> : null}
    </div>
    {actions != null ? <div className="ui-page-header-actions">{actions}</div> : null}
  </div>
);

const Stat = ({ value, unit, label, hint, accent, variant, className = "", ...rest }) => (
  <div className={`ui-stat${variant ? ` ${variant}` : ""} ${className}`} {...rest}>
    <div className="ui-stat-value" style={accent ? { color: accent } : undefined}>
      {value}{unit ? <span className="ui-stat-unit">{unit}</span> : null}
    </div>
    {label != null ? <div className="ui-stat-label">{label}</div> : null}
    {hint != null ? <div className="ui-stat-hint">{hint}</div> : null}
  </div>
);

const Toolbar = ({ children, className = "", ...rest }) => (
  <div className={`ui-toolbar ${className}`} {...rest}>{children}</div>
);

// ===== Theme hook + toggle ==================================
const useTheme = (defaultTheme = "light") => {
  const read = () => {
    try {
      const q = new URL(window.location.href).searchParams.get("theme");
      if (q === "light" || q === "dark") return q;
      const raw = localStorage.getItem("burner.theme");
      if (raw === "light" || raw === "dark") return raw;
    } catch {}
    return defaultTheme;
  };
  const [theme, setTheme] = React.useState(read);
  React.useEffect(() => {
    if (theme === "dark") document.documentElement.setAttribute("data-theme", "dark");
    else document.documentElement.removeAttribute("data-theme");
    try { localStorage.setItem("burner.theme", theme); } catch {}
  }, [theme]);
  return [theme, setTheme];
};

const ThemeToggle = ({ theme, onChange, size = "md", className = "" }) => {
  const next = theme === "dark" ? "light" : "dark";
  return (
    <button type="button" onClick={() => onChange(next)} className={`theme-toggle ${size === "sm" ? "theme-toggle-sm" : ""} ${className}`} aria-label={`Switch to ${next} mode`} title={`Switch to ${next} mode`}>
      {theme === "dark"
        ? <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="4" /><path d="M12 2v2" /><path d="M12 20v2" /><path d="m4.93 4.93 1.41 1.41" /><path d="m17.66 17.66 1.41 1.41" /><path d="M2 12h2" /><path d="M20 12h2" /><path d="m6.34 17.66-1.41 1.41" /><path d="m19.07 4.93-1.41 1.41" /></svg>
        : <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" /></svg>}
    </button>
  );
};
