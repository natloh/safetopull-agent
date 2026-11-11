"use strict";

const VERDICTS = {
  green: ["0x00", "SAFE"],
  yellow: ["0x01", "STEPS"],
  red: ["0x02", "HOLD"],
  unknown: ["0x--", "UNKNOWN"],
};
const SENT = [
  "App name, image reference, running and target version of each tracked app",
  "The verdict, its one-line summary and how many findings it has",
  "Agent version, CPU architecture, update mode and whether remote actions are on",
];
const state = {
  token: sessionStorage.getItem("stp-token") || "",
  locked: false,
  status: null,
  open: new Set(),
  notice: null,
  busy: false,
};

class Locked extends Error {}

function h(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(props || {})) {
    if (name === "class") node.className = value;
    else if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
    else if (value === true) node.setAttribute(name, "");
    else if (value !== false && value != null) node.setAttribute(name, value);
  }
  for (const child of children.flat()) {
    if (child != null && child !== false) node.append(child instanceof Node ? child : String(child));
  }
  return node;
}

async function api(path, body) {
  const headers = { Accept: "application/json" };
  const init = { headers };
  if (state.token) headers.Authorization = `Bearer ${state.token}`;
  if (body !== undefined) {
    init.method = "POST";
    init.body = JSON.stringify(body);
    headers["Content-Type"] = "application/json";
    headers["X-STP-Request"] = "1";
  }
  const response = await fetch(`/api${path}`, init);
  if (response.status === 401) {
    const rejected = state.token !== "";
    state.token = "";
    state.locked = true;
    sessionStorage.removeItem("stp-token");
    throw new Locked(rejected ? "That token was not accepted." : "This agent is protected by an API token.");
  }
  state.locked = false;
  const data = response.status === 204 ? null : await response.json().catch(() => null);
  if (!response.ok) throw new Error((data && data.error) || `Request failed (HTTP ${response.status}).`);
  return data;
}

async function act(work, done) {
  state.busy = true;
  state.notice = null;
  render();
  try {
    const result = await work();
    state.notice = { tone: "green", text: typeof done === "function" ? done(result) : done };
  } catch (error) {
    state.notice = { tone: "red", text: error.message };
  }
  state.busy = false;
  await load();
}

async function load() {
  try {
    state.status = await api("/status");
  } catch (error) {
    state.notice = { tone: error instanceof Locked ? "yellow" : "red", text: error.message };
  }
  render();
}

function when(seconds) {
  return seconds ? new Date(seconds * 1000).toLocaleString() : "never";
}

function pip(verdict, label) {
  const [code, text] = VERDICTS[verdict];
  return h("span", { class: `pip ${verdict}` }, `${code} // ${label || text}`);
}

function update(service) {
  const careful = service.verdict === "yellow";
  const question = careful
    ? `Update ${service.name} to ${service.target_version}?\n\nThis update has steps to follow first. Confirm only if you have read and handled them.`
    : `Update ${service.name} to ${service.target_version}? The container is recreated from its current settings.`;
  if (!window.confirm(question)) return;
  act(() => api("/update", { key: service.key, acknowledged: careful }), (result) => `${service.name} updated to ${result.updated_to}.`);
}

function serviceCard(service) {
  const open = state.open.has(service.key);
  const assessment = service.assessment;
  const title = service.stack ? `${service.stack} / ${service.name}` : service.name;
  const versions = [service.current_version || "unknown version", service.up_to_date ? null : service.target_version]
    .filter(Boolean)
    .join(" → ");
  const toggle = () => {
    if (open) state.open.delete(service.key);
    else state.open.add(service.key);
    render();
  };

  return h("article", { class: `panel service ${service.verdict}` },
    h("div", { class: "row" },
      h("div", { class: "stack" },
        h("h3", null, title),
        h("span", { class: "mono muted" }, `${service.app_name} · ${versions}`),
      ),
      pip(service.verdict, service.up_to_date ? "UP TO DATE" : null),
    ),
    h("p", null, h("strong", null, service.headline), service.summary ? ` — ${service.summary}` : ""),
    service.update_note && h("p", { class: "muted" }, service.update_note),
    h("div", { class: "row" },
      h("span", { class: "mono muted" }, service.image),
      h("span", { class: "row" },
        assessment && h("button", {
          "aria-expanded": String(open),
          "aria-label": `${open ? "Hide" : "Show"} steps for ${title}`,
          "data-focus": `steps:${service.key}`,
          onclick: toggle,
        }, open ? "Hide steps" : "Show steps"),
        service.can_update && h("button", {
          class: "primary",
          disabled: state.busy,
          "aria-label": `Update ${title} to ${service.target_version}`,
          "data-focus": `update:${service.key}`,
          onclick: () => update(service),
        }, `Update to ${service.target_version}`),
      ),
    ),
    open && assessment && h("div", { class: "stack" },
      h("h3", null, "What to do, in order"),
      h("ol", null, assessment.steps.map((step) => h("li", null,
        h("strong", null, step.title),
        step.detail && h("div", { class: "muted" }, step.detail),
        step.command && h("div", null, h("code", null, step.command)),
      ))),
      assessment.findings.length > 0 && h("h3", null, "Why"),
      assessment.findings.length > 0 && h("ul", null, assessment.findings.map((finding) => h("li", null,
        h("span", { class: "mono" }, `${finding.severity} · ${finding.release} · `),
        finding.explanation,
      ))),
    ),
  );
}

function cloudPanel(status) {
  const sent = h("ul", { class: "muted" }, SENT.map((line) => h("li", null, line)));

  if (status.cloud) {
    return h("section", { class: "panel" },
      h("div", { class: "row" },
        h("h2", null, "Cloud link"),
        h("button", { class: "danger", disabled: state.busy, onclick: () => {
          if (window.confirm("Unlink this agent? It keeps working locally and stops reporting.")) {
            act(() => api("/cloud/unlink", {}), "Unlinked. This agent no longer reports anywhere.");
          }
        } }, "Unlink"),
      ),
      h("p", null, "Linked to ", h("code", null, status.cloud.cloud_url), " as ", h("strong", null, status.cloud.host_name), "."),
      h("p", { class: "muted" }, "After each scan this agent sends exactly this, and nothing about untracked containers:"),
      sent,
      h("p", { class: "muted" }, status.remote_actions && status.update_mode === "apply"
        ? "Updates requested from the account are applied by this agent."
        : "Updates requested from the account are refused: remote actions are off on this host."),
    );
  }

  const submit = (event) => {
    event.preventDefault();
    const form = new FormData(event.target);
    act(() => api("/cloud/pair", { code: form.get("code"), name: form.get("name") }), (link) => `Linked as ${link.host_name}.`);
  };

  return h("section", { class: "panel" },
    h("h2", null, "Cloud link (optional)"),
    h("p", { class: "muted" }, "Not linked. This agent works on its own and reports nowhere. Link it only if you want to see several hosts in one account."),
    h("form", { onsubmit: submit },
      h("label", null, "Pairing code", h("input", { name: "code", required: true, autocomplete: "off", placeholder: "ABCDE-FGHJK" })),
      h("label", null, "Name for this host", h("input", { name: "name", required: true, maxlength: "100", placeholder: "nas" })),
      h("button", { class: "primary", type: "submit", disabled: state.busy }, "Link this host"),
    ),
    h("p", { class: "muted" }, "Links to ", h("code", null, status.default_cloud_url), ", set by STP_CLOUD_URL when the agent starts. If you link it, it sends only:"),
    sent,
  );
}

function unlockPanel() {
  const unlock = (event) => {
    event.preventDefault();
    state.token = new FormData(event.target).get("token").trim();
    sessionStorage.setItem("stp-token", state.token);
    state.notice = null;
    load();
  };

  return h("section", { class: "panel" },
    h("h2", null, "API token required"),
    h("p", { class: "muted" }, "This agent was started with STP_API_TOKEN. Enter it to view the dashboard. It is kept for this browser tab only."),
    h("form", { onsubmit: unlock },
      h("label", null, "API token", h("input", { name: "token", type: "password", required: true, autocomplete: "current-password" })),
      h("button", { class: "primary", type: "submit" }, "Unlock dashboard"),
    ),
  );
}

function render() {
  const root = document.getElementById("app");
  const status = state.status;
  const notice = state.notice && h("p", {
    class: `alert ${state.notice.tone}`,
    role: state.notice.tone === "red" ? "alert" : "status",
  }, state.notice.text);

  if (state.locked) {
    redraw(root, [notice, unlockPanel()]);
    return;
  }
  if (!status) {
    redraw(root, [notice || h("p", { class: "muted" }, "Reading what runs on this host…")]);
    return;
  }

  const snapshot = status.snapshot;
  const count = (verdict) => snapshot.services.filter((service) => service.verdict === verdict && !service.up_to_date).length;
  const current = snapshot.services.filter((service) => service.up_to_date).length;
  document.getElementById("meta").textContent =
    `v${status.agent_version} · ${status.arch} · ${status.update_mode === "apply" ? "applies updates when asked" : "advisory only"}`;

  redraw(root, [
    notice,
    snapshot.docker_error && h("p", { class: "alert red", role: "alert" }, `Cannot read Docker: ${snapshot.docker_error}`),
    snapshot.dataset_error && h("p", { class: "alert red", role: "alert" }, `No release data: ${snapshot.dataset_error}`),
    snapshot.dataset_stale && h("p", { class: "alert yellow" }, "The release dataset could not be refreshed, so verdicts use the last downloaded copy."),
    h("section", { class: "panel" },
      h("div", { class: "row" },
        h("h2", null, `${snapshot.services.length} tracked ${snapshot.services.length === 1 ? "app" : "apps"} on this host`),
        h("button", { disabled: state.busy, "data-focus": "refresh", onclick: () => act(() => api("/refresh", {}), "Checked just now.") }, "Check now"),
      ),
      h("div", { class: "row" },
        h("span", { class: "row" },
          pip("green", `${current} UP TO DATE`), pip("green", `${count("green")} SAFE`),
          pip("yellow", `${count("yellow")} STEPS`), pip("red", `${count("red")} HOLD`),
          count("unknown") > 0 && pip("unknown", `${count("unknown")} UNKNOWN`),
        ),
        h("span", { class: "mono muted" }, `last scan ${when(snapshot.scanned_at)}`),
      ),
      h("p", { class: "muted" }, `${snapshot.untracked_containers} other running ${snapshot.untracked_containers === 1 ? "container is" : "containers are"} not in the dataset. They are ignored and never reported.`),
    ),
    ...snapshot.services.map(serviceCard),
    cloudPanel(status),
    h("section", { class: "panel" },
      h("h2", null, "Network activity"),
      h("p", { class: "muted" }, "Release data comes from ", h("code", null, status.dataset_url), "."),
      h("div", { class: "scroll" }, h("table", null,
        h("caption", null, "Every request this agent made beyond this host since it started, newest first (last 50)."),
        h("thead", null, h("tr", null, ["When", "Why", "Request", "Status"].map((title) => h("th", { scope: "col" }, title)))),
        h("tbody", null, status.outbound.length === 0
          ? h("tr", null, h("td", { colspan: "4" }, "Nothing yet."))
          : status.outbound.map((record) => h("tr", null,
              h("td", null, when(record.at)), h("td", null, record.purpose),
              h("td", null, `${record.method} ${record.url}`), h("td", null, record.status == null ? "failed" : record.status),
            ))),
      )),
    ),
  ]);
}

/** Replaces the page content and puts keyboard focus back on the control that had it. */
function redraw(root, nodes) {
  const focused = document.activeElement instanceof HTMLElement ? document.activeElement.dataset.focus : undefined;
  root.replaceChildren(...nodes.filter(Boolean));
  if (focused) {
    const again = [...root.querySelectorAll("[data-focus]")].find((node) => node.dataset.focus === focused);
    if (again) again.focus();
  }
}

function editing() {
  const focused = document.activeElement;
  return focused instanceof HTMLElement && focused.closest("form") !== null;
}

load();
setInterval(() => {
  if (!state.busy && !state.locked && !document.hidden && !editing()) load();
}, 30000);
