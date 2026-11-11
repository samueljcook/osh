import type { EventEmitter } from "node:events";
import {
	createServer,
	type IncomingMessage,
	type RequestListener,
	type ServerResponse,
} from "node:http";
import type { Duplex } from "node:stream";
import { Effect } from "effect";
import { MODULE_SERVER_HOST } from "../config/vite-options.ts";
import { describeCause } from "../issues.ts";
import {
	DEV_RECONNECT_PATHNAME,
	type DevUpdateOutcome,
} from "./browser-source.ts";

/** The module server could not listen on its port, which another process took. */
export type DevModuleServerIssue = {
	readonly _tag: "DevModuleServerIssue";
	readonly port: number;
	readonly msg: string;
	readonly cause: unknown;
};

/** Where Vorma's module server listens; development documents load browser modules from its origin. */
export type DevModuleServerAddress = {
	readonly port: number;
	/** `http://127.0.0.1:<port>`. */
	readonly origin: string;
	/** `ws://127.0.0.1:<port>`, which Vite's hot update connection uses. */
	readonly webSocketOrigin: string;
};

export function createDevModuleServerAddress(
	port: number,
): DevModuleServerAddress {
	return {
		port,
		origin: `http://${MODULE_SERVER_HOST}:${port}`,
		webSocketOrigin: `ws://${MODULE_SERVER_HOST}:${port}`,
	};
}

/** A listener of the HTTP server's `upgrade` event, as Vite's WebSocket server registers it. */
type UpgradeListener = (
	request: IncomingMessage,
	socket: Duplex,
	head: Buffer,
) => void;

/** A listener of an event of the HTTP server. */
type EventListener = Parameters<EventEmitter["on"]>[1];

/** A request that the module server holds during Vorma's restart of Vite. */
type HeldRequest = {
	readonly request: IncomingMessage;
	readonly response: ServerResponse;
};

/** A WebSocket upgrade that the module server holds while no Vite server is active. */
type HeldUpgrade = {
	readonly request: IncomingMessage;
	readonly socket: Duplex;
	readonly head: Buffer;
};

/**
 * The `upgrade` listeners of Vite's servers. Each Vite server registers its
 * listener when it creates its WebSocket server, before Vite configures and
 * starts that server, so a new listener is a candidate until Vorma activates
 * it. A later registration replaces a candidate that never became active.
 * While no listener is active, the module server holds the upgrades that
 * arrive and hands them to the next listener that Vorma activates.
 */
type ViteUpgradeListeners =
	| { readonly kind: "none"; readonly held: Array<HeldUpgrade> }
	| {
			readonly kind: "candidate";
			readonly candidate: UpgradeListener;
			readonly held: Array<HeldUpgrade>;
	  }
	| { readonly kind: "active"; readonly active: UpgradeListener }
	| {
			readonly kind: "active-and-candidate";
			readonly active: UpgradeListener;
			readonly candidate: UpgradeListener;
	  };

/**
 * The imports of the reconnect module (see `DEV_RECONNECT_PATHNAME`).
 * `answering`: the latest update completed and no restart of Vite began
 * since, so an import receives that completion at once. `waiting`: the
 * session's first update has not ended, a restart of Vite began, or the
 * latest update failed, so each import waits for the end of the next update.
 */
type ReconnectImports =
	| { readonly kind: "answering" }
	| { readonly kind: "waiting"; readonly responses: Array<ServerResponse> };

/**
 * Answers an upgrade that arrives or waits when the module server closes.
 * Vite's client does not open a refused hot update connection again: it
 * reports that it failed to connect. Its ping, which also arrives as an
 * upgrade, tries again every second.
 */
function refuseUpgrade(socket: Duplex): void {
	socket.end(
		"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
	);
}

function testReconnectRequest(request: IncomingMessage): boolean {
	return request.url?.split("?", 1)[0] === DEV_RECONNECT_PATHNAME;
}

/** Ends the answer to an import of the reconnect module with the module that carries `outcome`. */
function endReconnectModule(
	response: ServerResponse,
	outcome: DevUpdateOutcome,
): void {
	if (!response.destroyed) {
		response.end(`export const outcome = ${JSON.stringify(outcome)};\n`);
	}
}

/**
 * Hosts only Vite middleware and its WebSocket; the application has its own
 * server. `holdRequests` keeps every request that arrives, except the imports
 * of the reconnect module, from reaching Vite until `endUpdate`, which passes
 * them to Vite in their order.
 *
 * Vite registers its WebSocket server's `upgrade` listener on this server with
 * `on` and removes it with `off`. The module server keeps those listeners out
 * of its own event dispatch: its one `upgrade` listener hands each upgrade to
 * the listener of the Vite server that is active when the upgrade arrives,
 * also while it holds requests, so the WebSocket server of a Vite server that
 * a failed restart created never receives a connection.
 * `activateUpgradeCandidate` makes the latest registered listener the active
 * one. An upgrade that arrives while no Vite server is active waits for the
 * next activation.
 *
 * The module server also answers the imports of the reconnect module, through
 * which a browser that lost its connection to Vite learns the outcome of each
 * update (see `DEV_RECONNECT_PATHNAME` for the protocol). It passes these
 * requests to Vite also while it holds requests, so that Vite's CORS and host
 * checks run first; Vorma's Vite plugin then calls `answerReconnect`. An
 * import waits before the session's first update ends, after a restart of
 * Vite began (`holdRequests` for Vorma's restart, `holdReconnects` for a
 * plugin's), and after an update failed, and `endUpdate` answers the imports
 * that wait with the outcome of the update that ends. The session calls `endUpdate` for every update that completes or
 * fails, and not for an update that server work supersedes.
 */
export function createDevModuleServer(address: DevModuleServerAddress) {
	return Effect.gen(function* () {
		let handling:
			| { readonly kind: "unavailable" }
			| { readonly kind: "serving"; readonly handler: RequestListener }
			| {
					readonly kind: "holding";
					readonly handler: RequestListener;
					readonly held: Array<HeldRequest>;
			  }
			| { readonly kind: "closed" } = { kind: "unavailable" };
		let upgradeListeners: ViteUpgradeListeners = { kind: "none", held: [] };
		let reconnects: ReconnectImports = { kind: "waiting", responses: [] };
		function dispatchUpgrade(
			request: IncomingMessage,
			socket: Duplex,
			head: Buffer,
		) {
			// Node's HTTP server leaves an upgraded socket without an error
			// listener, and a client can reset the connection at any time.
			socket.on("error", () => undefined);
			if (handling.kind === "closed") {
				refuseUpgrade(socket);
				return;
			}
			const listeners = upgradeListeners;
			if (
				listeners.kind === "active" ||
				listeners.kind === "active-and-candidate"
			) {
				listeners.active(request, socket, head);
				return;
			}
			listeners.held.push({ request, socket, head });
		}
		function registerUpgradeListener(listener: UpgradeListener) {
			const listeners = upgradeListeners;
			upgradeListeners =
				listeners.kind === "none" || listeners.kind === "candidate"
					? {
							kind: "candidate",
							candidate: listener,
							held: listeners.held,
						}
					: {
							kind: "active-and-candidate",
							active: listeners.active,
							candidate: listener,
						};
		}
		function forgetUpgradeListener(listener: UpgradeListener) {
			const listeners = upgradeListeners;
			if (
				listeners.kind === "candidate" &&
				listeners.candidate === listener
			) {
				upgradeListeners = { kind: "none", held: listeners.held };
			} else if (
				listeners.kind === "active" &&
				listeners.active === listener
			) {
				upgradeListeners = { kind: "none", held: [] };
			} else if (listeners.kind === "active-and-candidate") {
				if (listeners.active === listener) {
					upgradeListeners = {
						kind: "candidate",
						candidate: listeners.candidate,
						held: [],
					};
				} else if (listeners.candidate === listener) {
					upgradeListeners = {
						kind: "active",
						active: listeners.active,
					};
				}
			}
		}
		const server = yield* Effect.acquireRelease(
			Effect.sync(() => {
				const created = createServer((request, response) => {
					if (
						handling.kind === "serving" ||
						(handling.kind === "holding" &&
							testReconnectRequest(request))
					) {
						handling.handler(request, response);
					} else if (handling.kind === "holding") {
						handling.held.push({ request, response });
					} else {
						response.writeHead(503).end();
					}
				});
				created.on("upgrade", dispatchUpgrade);
				const addServerListener = created.on.bind(created);
				const removeServerListener = created.off.bind(created);
				// Vite calls only `on` and `off` on the server of its WebSocket,
				// each with `upgrade`; other events keep their own dispatch.
				return Object.assign(created, {
					on(event: string | symbol, listener: EventListener) {
						if (event === "upgrade") {
							registerUpgradeListener(
								listener as UpgradeListener,
							);
							return created;
						}
						return addServerListener(event, listener);
					},
					off(event: string | symbol, listener: EventListener) {
						if (event === "upgrade") {
							forgetUpgradeListener(listener as UpgradeListener);
							return created;
						}
						return removeServerListener(event, listener);
					},
				});
			}),
			(server) => {
				return Effect.callback<void>((resume) => {
					const previous = handling;
					handling = { kind: "closed" };
					if (previous.kind === "holding") {
						for (const held of previous.held) {
							held.response.writeHead(503).end();
						}
					}
					const listeners = upgradeListeners;
					if (
						listeners.kind === "none" ||
						listeners.kind === "candidate"
					) {
						for (const held of listeners.held) {
							refuseUpgrade(held.socket);
						}
					}
					const previousReconnects = reconnects;
					reconnects = { kind: "waiting", responses: [] };
					if (previousReconnects.kind === "waiting") {
						// The headers left already, so the import fails only when
						// the connection ends.
						for (const response of previousReconnects.responses) {
							response.destroy();
						}
					}
					server.close(() => resume(Effect.void));
					server.closeAllConnections();
				});
			},
		);
		const listen = Effect.callback<void, DevModuleServerIssue>((resume) => {
			function failListen(cause: unknown) {
				resume(
					Effect.fail({
						_tag: "DevModuleServerIssue",
						port: address.port,
						msg: `Vorma could not listen on ${MODULE_SERVER_HOST}:${address.port} for its module server: ${describeCause(cause)}. Vorma reserves another port and starts Vite again.`,
						cause,
					}),
				);
			}
			server.once("error", failListen);
			server.listen(address.port, MODULE_SERVER_HOST, () => {
				server.off("error", failListen);
				resume(Effect.void);
			});
		});
		/** Makes the imports of the reconnect module wait for the end of the next update. */
		function holdReconnects() {
			if (reconnects.kind === "answering") {
				reconnects = { kind: "waiting", responses: [] };
			}
		}
		return {
			server,
			listen,
			origin: address.origin,
			webSocketOrigin: address.webSocketOrigin,
			port: address.port,
			setHandler(next: RequestListener) {
				if (handling.kind === "unavailable") {
					handling = { kind: "serving", handler: next };
				}
			},
			/**
			 * Makes the latest registered Vite server's `upgrade` listener the one
			 * that receives WebSocket upgrades, and hands it the upgrades that
			 * arrived while no listener was active.
			 */
			activateUpgradeCandidate() {
				const listeners = upgradeListeners;
				if (listeners.kind === "candidate") {
					upgradeListeners = {
						kind: "active",
						active: listeners.candidate,
					};
					for (const held of listeners.held) {
						if (!held.socket.destroyed) {
							listeners.candidate(
								held.request,
								held.socket,
								held.head,
							);
						}
					}
				} else if (listeners.kind === "active-and-candidate") {
					upgradeListeners = {
						kind: "active",
						active: listeners.candidate,
					};
				}
			},
			/** Holds requests for Vorma's restart of Vite, and makes the imports of the reconnect module wait. */
			holdRequests() {
				if (handling.kind === "serving") {
					handling = {
						kind: "holding",
						handler: handling.handler,
						held: [],
					};
				}
				holdReconnects();
			},
			/** Makes the imports of the reconnect module wait, for a restart of Vite that a plugin began. */
			holdReconnects,
			/** Whether a browser waits for the outcome of an update. */
			testReconnectsWaiting(): boolean {
				return (
					reconnects.kind === "waiting" &&
					reconnects.responses.some((response) => !response.destroyed)
				);
			},
			/**
			 * Answers an import of the reconnect module that passed Vite's CORS
			 * and host checks. The headers leave at once; the module follows now
			 * or when the next update ends.
			 */
			answerReconnect(response: ServerResponse) {
				if (handling.kind === "closed") {
					response.writeHead(503).end();
					return;
				}
				response.writeHead(200, {
					"content-type": "text/javascript",
					"cache-control": "no-store",
				});
				response.flushHeaders();
				if (reconnects.kind === "answering") {
					endReconnectModule(response, { kind: "completed" });
					return;
				}
				reconnects.responses.push(response);
			},
			/**
			 * Ends an update that completed or failed: passes the held requests
			 * to Vite in their order, and answers the imports of the reconnect
			 * module that wait with `outcome`. After a failed update, later
			 * imports wait for the next update.
			 */
			endUpdate(outcome: DevUpdateOutcome) {
				if (handling.kind === "closed") {
					return;
				}
				if (handling.kind === "holding") {
					const { handler, held } = handling;
					handling = { kind: "serving", handler };
					for (const item of held) {
						handler(item.request, item.response);
					}
				}
				const waiting =
					reconnects.kind === "waiting" ? reconnects.responses : [];
				reconnects =
					outcome.kind === "completed"
						? { kind: "answering" }
						: { kind: "waiting", responses: [] };
				for (const response of waiting) {
					endReconnectModule(response, outcome);
				}
			},
		};
	});
}
