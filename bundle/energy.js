// The console's Energy page: the snapshot this plugin publishes under
// `plugins.powermetrics-plugin` in `/api/status`, read every 5 seconds, plus
// the board power of the last minutes as an SVG line.

const PLUGIN = "powermetrics-plugin";
const POLL_MS = 5000;
// Ten minutes of readings at one per poll.
const HISTORY = 120;
const SVG_NS = "http://www.w3.org/2000/svg";

// The host imports this module and calls registerMeshPluginUi(host).
const moduleRegistration = {
  async registerMeshPluginUi() {
    return { pages: { energy: mountEnergyPage } };
  },
};
export const registerMeshPluginUi = moduleRegistration.registerMeshPluginUi;

function el(tag, style, text) {
  const node = document.createElement(tag);
  Object.assign(node.style, style);
  if (text !== undefined) node.textContent = text;
  return node;
}

function significant(value, digits = 3) {
  if (value === 0) return "0";
  return Number(value.toPrecision(digits)).toLocaleString(undefined, {
    maximumFractionDigits: 6,
  });
}

function money(value, currency) {
  if (currency && /^[A-Z]{3}$/.test(currency)) {
    try {
      return new Intl.NumberFormat(undefined, {
        style: "currency",
        currency,
        maximumSignificantDigits: 3,
      }).format(value);
    } catch {
      // An ISO-shaped code the browser does not know: plain number below.
    }
  }
  return `${significant(value)}${currency ? ` ${currency}` : ""}`;
}

function duration(seconds) {
  if (seconds < 90) return `${Math.round(seconds)} s`;
  if (seconds < 5400) return `${Math.round(seconds / 60)} min`;
  return `${significant(seconds / 3600, 2)} h`;
}

// The console titles the page itself, so it starts with the status line.
function mountEnergyPage({ element, host }) {
  const { tokens } = host.appearance;
  Object.assign(element.style, {
    display: "grid",
    gap: "18px",
    maxWidth: "860px",
    padding: "4px",
  });

  const status = el("p", { color: tokens.foreground, margin: "0", opacity: "0.8" }, "Waiting for the first reading…");
  status.setAttribute("role", "status");

  const grid = el("div", {
    display: "grid",
    gap: "12px",
    gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
  });
  const cards = {};
  function card(key, label, hint) {
    const box = el("section", {
      background: tokens.panelStrong,
      border: `1px solid ${tokens.border}`,
      borderRadius: tokens.radiusLarge,
      display: "grid",
      gap: "4px",
      padding: "14px 16px",
    });
    box.setAttribute("aria-label", label);
    const value = el("p", { color: tokens.accent, fontSize: "1.5rem", fontWeight: "700", margin: "0" }, "–");
    value.dataset.metric = key;
    box.append(
      el("h3", { color: tokens.foreground, fontSize: "0.8125rem", fontWeight: "600", margin: "0" }, label),
      value,
      el("p", { color: tokens.foreground, fontSize: "0.75rem", margin: "0", opacity: "0.7" }, hint),
    );
    cards[key] = { box, value };
    grid.append(box);
  }
  card("watts", "Board power now", "Summed over the GPUs");
  card("idle", "Idle draw", "Mean power while no request runs");
  card("marginal", "Energy per 1,000 tokens", "On top of the idle draw");
  card("total", "Energy per 1,000 tokens, idle included", "What serving costs a machine that runs only for it");
  card("cost", "Cost per million tokens", "On top of the idle draw");
  card("costTotal", "Cost per million tokens, idle included", "Idle draw included");
  card("tokens", "Output tokens measured", "Requests whose whole run was measured");
  card("time", "Measured time", "Busy and idle");

  const chartPanel = el("section", {
    background: tokens.panelStrong,
    border: `1px solid ${tokens.border}`,
    borderRadius: tokens.radiusLarge,
    display: "grid",
    gap: "8px",
    padding: "14px 16px",
  });
  chartPanel.setAttribute("aria-label", "Board power over the last minutes");
  const chartTitle = el(
    "h3",
    { color: tokens.foreground, fontSize: "0.8125rem", fontWeight: "600", margin: "0" },
    "Board power, last 10 minutes",
  );
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${HISTORY - 1} 100`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("role", "img");
  Object.assign(svg.style, { display: "block", height: "120px", width: "100%" });
  const idleLine = document.createElementNS(SVG_NS, "line");
  idleLine.setAttribute("x1", "0");
  idleLine.setAttribute("x2", String(HISTORY - 1));
  idleLine.setAttribute("stroke", tokens.warn);
  idleLine.setAttribute("stroke-dasharray", "3 3");
  idleLine.setAttribute("stroke-width", "1");
  idleLine.setAttribute("vector-effect", "non-scaling-stroke");
  idleLine.style.display = "none";
  const line = document.createElementNS(SVG_NS, "polyline");
  line.setAttribute("fill", "none");
  line.setAttribute("stroke", tokens.accent);
  line.setAttribute("stroke-width", "2");
  line.setAttribute("stroke-linejoin", "round");
  line.setAttribute("vector-effect", "non-scaling-stroke");
  svg.append(idleLine, line);
  const chartScale = el("p", { color: tokens.foreground, fontSize: "0.75rem", margin: "0", opacity: "0.7" }, "");
  chartPanel.append(chartTitle, svg, chartScale);

  element.append(status, grid, chartPanel);

  const history = [];
  let lastReading = null;

  function show(key, text) {
    cards[key].value.textContent = text;
  }

  function draw(idleWatts) {
    if (history.length === 0) return;
    const top = Math.max(...history, idleWatts ?? 0) * 1.1 || 1;
    const offset = HISTORY - history.length;
    const y = (watts) => (100 - (watts / top) * 100).toFixed(2);
    line.setAttribute("points", history.map((watts, i) => `${offset + i},${y(watts)}`).join(" "));
    if (idleWatts != null) {
      idleLine.setAttribute("y1", y(idleWatts));
      idleLine.setAttribute("y2", y(idleWatts));
      idleLine.style.display = "";
    }
    svg.setAttribute("aria-label", `Board power, latest ${significant(history[history.length - 1])} W`);
    chartScale.textContent = `0 to ${significant(top, 2)} W${idleWatts != null ? ", dashed line: idle draw" : ""}`;
  }

  function render(entry) {
    if (!entry) {
      status.textContent = `${PLUGIN} has published nothing yet. Readings start once the node runs.`;
      return;
    }
    const data = entry.data ?? {};
    const state = entry.state === "ready" ? "live" : entry.state;
    status.textContent = `Source: ${data.source ?? "unknown"} · ${state}`;
    status.style.color = entry.state === "ready" ? tokens.foreground : tokens.warn;

    if (typeof data.watts === "number" && data.last_reading_unix_ns > (lastReading ?? 0)) {
      lastReading = data.last_reading_unix_ns;
      history.push(data.watts);
      if (history.length > HISTORY) history.shift();
    }
    show("watts", typeof data.watts === "number" ? `${significant(data.watts)} W` : "–");
    show("idle", data.idle_watts != null ? `${significant(data.idle_watts)} W` : "measuring…");
    show("marginal", data.wh_per_1k_output_tokens != null ? `${significant(data.wh_per_1k_output_tokens)} Wh` : "after a request");
    show("total", data.wh_per_1k_output_tokens_total != null ? `${significant(data.wh_per_1k_output_tokens_total)} Wh` : "after a request");
    const priced = typeof data.electricity_price_per_kwh === "number";
    cards.cost.box.style.display = priced ? "" : "none";
    cards.costTotal.box.style.display = priced ? "" : "none";
    if (priced) {
      show("cost", data.cost_per_1m_output_tokens != null ? money(data.cost_per_1m_output_tokens, data.currency) : "after a request");
      show("costTotal", data.cost_per_1m_output_tokens_total != null ? money(data.cost_per_1m_output_tokens_total, data.currency) : "after a request");
    }
    show("tokens", (data.output_tokens ?? 0).toLocaleString());
    show("time", `${duration(data.busy_seconds ?? 0)} / ${duration(data.idle_seconds ?? 0)}`);
    draw(data.idle_watts);
  }

  // One request at a time, given up after a poll interval, and the next one
  // scheduled only once it settles, so a stalled host never piles requests up.
  let stopped = false;
  let timer = null;
  let inFlight = null;
  async function poll() {
    inFlight = new AbortController();
    const giveUp = setTimeout(() => inFlight.abort(), POLL_MS);
    try {
      const response = await fetch("/api/status", {
        headers: { accept: "application/json" },
        signal: inFlight.signal,
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const body = await response.json();
      if (!stopped) render(body?.plugins?.[PLUGIN]);
    } catch (error) {
      if (!stopped) {
        status.textContent = `Status unavailable: ${error.name === "AbortError" ? "no answer" : error.message}`;
        status.style.color = tokens.bad;
      }
    } finally {
      clearTimeout(giveUp);
      inFlight = null;
      if (!stopped) timer = setTimeout(poll, POLL_MS);
    }
  }
  poll();

  return {
    unmount() {
      stopped = true;
      clearTimeout(timer);
      inFlight?.abort();
      element.replaceChildren();
    },
  };
}
