import {
	DEV_CONTROLS_PROTOCOL,
	DOCUMENT_PROTOCOL,
} from "@vorma/client/unstable";

export const DEV_BROWSER_MODULE = "\0vorma:development-browser";

/**
 * The path of the reconnect module on the module server. The browser module
 * imports it after the browser loses its connection to Vite, which happens
 * when a restart of Vite closes the server that the browser was connected to.
 * The module server answers each import with the outcome of the update that
 * ends after the import arrived:
 *
 * - When that update completes, the module exports `{ kind: "completed" }`.
 *   The browser module returns, and Vite's client pings the module server and
 *   reloads the document, so the document loads only after an update has
 *   completed.
 * - When that update fails, at any step and whether or not it restarted Vite,
 *   the module exports `{ kind: "failed", msg }`. The browser module shows the
 *   failure as it shows a failure that arrives over the connection, and then
 *   imports the reconnect module again, which the module server answers when
 *   the next update ends. The browser therefore never reloads into the state
 *   of a failed update, including an application that the update stopped.
 * - An update that server work supersedes does not end, so the import waits
 *   for the update that replaces it.
 *
 * While the latest update has completed and no restart of Vite has begun
 * since, the module server answers an import with `{ kind: "completed" }` at
 * once. The module server sends the answer's headers when the import arrives
 * so that no browser stops waiting for them during a long failure. When the
 * module server closes, at the end of the session, it ends the imports that
 * wait, which then fail; the browser module returns, and Vite's client pings
 * until a module server answers and then reloads the document.
 */
export const DEV_RECONNECT_PATHNAME = "/__vorma/development-reconnect";

/** What the reconnect module exports as `outcome`; see `DEV_RECONNECT_PATHNAME`. */
export type DevUpdateOutcome =
	| { readonly kind: "completed" }
	| { readonly kind: "failed"; readonly msg: string };

/** The Vite HMR event that carries the session's messages to the browser module below. */
export const DEV_BROWSER_EVENT = "vorma:development";

/** Acknowledges an invalidation after Vite has sent its resulting updates or reload. */
export const DEV_INVALIDATION_ACK_EVENT = "vorma:invalidation-processed";

/**
 * What the session tells the browser. `full-reload` is Vite's own reload
 * message; the other kinds travel on `DEV_BROWSER_EVENT` and the
 * browser module below handles each one.
 */
export type DevBrowserMsg =
	| { readonly kind: "full-reload" }
	| { readonly kind: "rebuilding" }
	| { readonly kind: "error"; readonly msg: string }
	| { readonly kind: "css"; readonly css: string }
	| { readonly kind: "revalidate" }
	| { readonly kind: "ready" };

/** Browser-only development controls use Vite's authenticated HMR connection. */
export function renderDevBrowserSource(input: {
	readonly refreshRuntime: "react" | "preact" | "solid" | undefined;
	/** The module server URL of `DEV_RECONNECT_PATHNAME`. */
	readonly reconnectModuleUrl: string;
}) {
	return String.raw`
${input.refreshRuntime === "react" ? 'import "/@react-refresh";' : ""}
const viewModules = new Set();
let lifetime = { kind: "active" };
let updates = { kind: "idle" };
let status = { kind: "ready" };
let revalidations = { kind: "idle" };
function holdUpdates() {
  if (updates.kind === "disposed" || updates.kind === "reloading") return;
  if (updates.kind === "idle") {
    updates = {
      kind: "holding",
      pendingUpdateCount: 0,
      invalidations: new Set(),
      views: new Map(),
      moduleKinds: new Set(),
      completion: Promise.withResolvers(),
    };
  }
  return updates;
}
function finishUpdates() {
  if (updates.kind !== "holding" || updates.pendingUpdateCount !== 0 || updates.invalidations.size !== 0) return;
  const finished = updates;
  updates = { kind: "idle" };
  for (const [url, module] of finished.views) {
    window[${JSON.stringify(DEV_CONTROLS_PROTOCOL.windowProperty)}]?.updateModuleFn(url, module);
  }
  finished.completion.resolve();
}
export function registerViewModule(url) {
  // Vite identifies hot-update modules by decoded paths.
  if (lifetime.kind === "active") viewModules.add(decodeURI(new URL(url).pathname));
}
export function updateViewModule(url, module) {
  holdUpdates()?.views.set(url, module);
}
${input.refreshRuntime === "react" ? 'window.__registerBeforePerformReactRefresh(() => updates.kind === "holding" ? updates.completion.promise : updates.kind === "reloading" ? updates.completion : undefined);' : ""}
function ensureOverlayElement(failed) {
  const element = status.kind === "ready" ? document.createElement("div") : status.element;
  if (status.kind === "ready") {
    element.id = "vorma-rebuilding-overlay";
    element.style.cssText = "position:fixed;bottom:16px;right:16px;z-index:2147483647;color:white;padding:10px 16px;border-radius:4px;font:14px system-ui";
    document.body.append(element);
  }
  element.textContent = failed ? "Build error. See the console for details." : "Rebuilding…";
  element.setAttribute("role", failed ? "alert" : "status");
  element.style.backgroundColor = failed ? "#700" : "#222";
  return element;
}
function clearOverlayStatus() {
  const previous = status;
  status = { kind: "ready" };
  if (previous.kind !== "ready") previous.element.remove();
}
function syncReadyStatus() {
  if (lifetime.kind !== "active") return;
  if (revalidations.kind === "active") {
    status = { kind: "ready-revalidating", element: ensureOverlayElement(false) };
  } else {
    clearOverlayStatus();
  }
}
function showBuildError(msg) {
  if (lifetime.kind !== "active") return;
  console.error("[vorma] Build error", msg);
  status = { kind: "error", element: ensureOverlayElement(true) };
}
let reconnectImports = 0;
// After the browser lost its connection to Vite, this function imports the
// reconnect module. The module server answers the import with the outcome of
// the next update that ends (an update that server work supersedes does not
// end), or with the latest update's completion at once when no restart of
// Vite began since that update completed. When the update completed, this
// function returns, and Vite's client pings the module server and reloads the
// document. When the update failed, this function shows the failure and
// imports the module again, which waits for the next update, so the browser
// never reloads into the state of a failed update. A failed import means that
// the module server closed, and Vite's client then pings until a module server
// answers. Each import has a new URL, so the browser never reuses an earlier
// answer.
function waitForCompletedUpdate() {
  reconnectImports += 1;
  return import(/* @vite-ignore */ ${JSON.stringify(input.reconnectModuleUrl)} + "?t=" + Date.now() + "-" + reconnectImports).then(
    ({ outcome }) => {
      if (outcome.kind === "completed") return;
      showBuildError(outcome.msg);
      return waitForCompletedUpdate();
    },
    () => {},
  );
}
if (import.meta.hot) {
  // Invalidation can propagate through several messages. Vite acknowledges each
  // invalidation after sending its updates, so a renderer cannot refresh between them.
  import.meta.hot.on("vite:beforeUpdate", (batch) => {
    const held = holdUpdates();
    if (!held) return;
    // Vite's client notifies afterUpdate once for each update that it notified beforeUpdate for.
    held.pendingUpdateCount += 1;
    let reloadReason;
    for (const update of batch.updates) {
      if (update.type !== "js-update") continue;
      if (viewModules.has(update.acceptedPath)) {
        held.moduleKinds.add("views");
        // A document reload uses Vorma's encoded module URLs for literal percent signs.
        if (update.acceptedPath.includes("%")) reloadReason = "a view filename contains a literal percent sign";
      } else held.moduleKinds.add("other");
    }
    // Shared renderer and view updates require the same module identities.
    if (${input.refreshRuntime !== undefined} && held.moduleKinds.has("views") && held.moduleKinds.has("other")) {
      reloadReason = "this update affects both route views and renderer refresh modules";
    }
    if (reloadReason) {
      const completion = new Promise(() => {});
      updates = { kind: "reloading", completion };
      console.info("[vorma] Reloading because " + reloadReason + ".");
      window.location.reload();
      return completion;
    }
  });
  import.meta.hot.on("vite:afterUpdate", () => {
    if (updates.kind !== "holding" || updates.pendingUpdateCount === 0) return;
    updates.pendingUpdateCount -= 1;
    finishUpdates();
  });
  // Vite's client waits for this listener, and then pings the module server
  // and reloads the document.
  import.meta.hot.on("vite:ws:disconnect", waitForCompletedUpdate);
  import.meta.hot.on("vite:invalidate", ({ path }) => { holdUpdates()?.invalidations.add(path); });
  import.meta.hot.on(${JSON.stringify(DEV_INVALIDATION_ACK_EVENT)}, ({ path }) => {
    if (updates.kind !== "holding") return;
    updates.invalidations.delete(path);
    finishUpdates();
  });
  import.meta.hot.on(${JSON.stringify(DEV_BROWSER_EVENT)}, (msg) => {
    if (lifetime.kind !== "active") return;
    if (msg.kind === "rebuilding") {
      status = { kind: "rebuilding", element: ensureOverlayElement(false) };
    } else if (msg.kind === "error") {
      showBuildError(msg.msg);
    } else if (msg.kind === "ready") {
      syncReadyStatus();
    } else if (msg.kind === "css") {
      let style = document.getElementById(${JSON.stringify(DOCUMENT_PROTOCOL.elementIds.criticalCss)});
      if (!style) {
        style = document.createElement("style");
        style.id = ${JSON.stringify(DOCUMENT_PROTOCOL.elementIds.criticalCss)};
        document.head.append(style);
      }
      // A page with a strict style policy permits the new text only from an
      // element that has Vite's nonce, which the head carries in a meta element.
      const nonce = document.querySelector("meta[property=csp-nonce]")?.nonce;
      if (nonce) style.nonce = nonce;
      style.textContent = msg.css;
    } else if (msg.kind === "revalidate") {
      const owner = lifetime;
      const task = {};
      if (revalidations.kind === "idle") revalidations = { kind: "active", tasks: new Set() };
      const active = revalidations;
      active.tasks.add(task);
      syncReadyStatus();
      // Vite awaits event handlers; a data request must not block later CSS or build messages.
      Promise.resolve().then(() => window[${JSON.stringify(DEV_CONTROLS_PROTOCOL.windowProperty)}]?.revalidateFn()).then(
        () => {},
        (cause) => console.error("[vorma] Data revalidation failed", cause),
      ).then(() => {
        if (lifetime !== owner) return;
        active.tasks.delete(task);
        if (revalidations === active && active.tasks.size === 0) revalidations = { kind: "idle" };
        if (status.kind === "ready-revalidating" && revalidations.kind === "idle") clearOverlayStatus();
      });
    }
  });
  import.meta.hot.dispose(() => {
    lifetime = { kind: "disposed" };
    const previous = updates;
    updates = { kind: "disposed" };
    revalidations = { kind: "idle" };
    clearOverlayStatus();
    if (previous.kind === "holding") previous.completion.resolve();
  });
}
`;
}
