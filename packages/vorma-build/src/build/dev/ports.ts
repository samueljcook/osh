import { NodeSocketServer } from "@effect/platform-node-shared";
import { Effect, Result } from "effect";
import { MODULE_SERVER_HOST } from "../config/vite-options.ts";
import { describeCause } from "../issues.ts";

/** Where `vorma dev` starts looking for the application server's port when the configuration sets no `dev.server.preferredPort`. */
export const APPLICATION_PREFERRED_PORT = 8080;

/** Where `vorma dev` starts looking for its module server's port: Vite's own default. */
export const MODULE_SERVER_PREFERRED_PORT = 5173;

/** The preferred port and the 1,024 ports after it are searched in order. */
const SEARCHED_PORT_COUNT = 1_025;

const HIGHEST_PORT = 65_535;

/** Vorma could not reserve a port that it needs. */
export type DevPortIssue = {
	readonly _tag: "DevPortIssue";
	readonly port: number;
	readonly msg: string;
	readonly cause: unknown;
};

/**
 * Which addresses a reservation binds. The module server listens on the
 * loopback address only, so its reservation binds only that address. The
 * application chooses its own address, so its reservation binds every
 * address.
 */
export type PortReservationScope = "loopback" | "every-address";

/**
 * Reserves the preferred port when it is free, otherwise the first free port
 * among the 1,024 ports after it, otherwise a port that the operating system
 * assigns. The reservation listens until the enclosing scope closes, so a
 * concurrent `vorma dev` process chooses a different port. Close the scope
 * just before the port's owner starts listening.
 */
export function reserveFirstFreePort(input: {
	readonly preferredPort: number;
	readonly reservationScope: PortReservationScope;
}) {
	return Effect.gen(function* () {
		const lastPort = Math.min(
			input.preferredPort + SEARCHED_PORT_COUNT - 1,
			HIGHEST_PORT,
		);
		for (let port = input.preferredPort; port <= lastPort; port++) {
			const reserved = yield* reserveExactPort({
				port,
				reservationScope: input.reservationScope,
			}).pipe(Effect.result);
			if (Result.isSuccess(reserved)) {
				return port;
			}
		}
		const { address } = yield* NodeSocketServer.make({
			port: 0,
			...(input.reservationScope === "loopback"
				? { host: MODULE_SERVER_HOST }
				: {}),
		}).pipe(
			Effect.mapError((cause): DevPortIssue => {
				return {
					_tag: "DevPortIssue",
					port: 0,
					msg: `Vorma could not reserve a port that the operating system assigns: ${describeCause(cause)}`,
					cause,
				};
			}),
		);
		// A numeric listen port without a socket path always creates a TCP listener.
		if (address._tag === "UnixPathAddress") {
			throw new Error(
				"A TCP port reservation returned a Unix socket address",
			);
		}
		return address.port;
	});
}

/**
 * Reserves exactly this port, or fails when it is not free. A port is free
 * when it can be bound on the loopback address and, for a reservation of
 * every address, then on every address. The loopback check of such a
 * reservation is released at once; the binding on every address remains as
 * the reservation until the enclosing scope closes.
 *
 * A reservation accepts connections and holds them without an answer until it
 * closes. For the application's port this has two effects: a browser request
 * to the application during a rebuild waits and then fails when the
 * reservation closes, and another machine on the network can connect to the
 * port while Vorma holds it. Vorma still reserves the application's port so
 * that a concurrent `vorma dev` process cannot take it during a rebuild; Node
 * cannot bind a port without accepting connections on it.
 */
export function reserveExactPort(input: {
	readonly port: number;
	readonly reservationScope: PortReservationScope;
}) {
	const { port } = input;
	const loopback = NodeSocketServer.make({ host: MODULE_SERVER_HOST, port });
	return (
		input.reservationScope === "loopback"
			? loopback
			: Effect.scoped(loopback).pipe(
					Effect.andThen(NodeSocketServer.make({ port })),
				)
	).pipe(
		Effect.asVoid,
		Effect.mapError((cause): DevPortIssue => {
			return {
				_tag: "DevPortIssue",
				port,
				msg: `Vorma could not reserve port ${port}: ${describeCause(cause)}. Stop the process that listens on it.`,
				cause,
			};
		}),
	);
}
