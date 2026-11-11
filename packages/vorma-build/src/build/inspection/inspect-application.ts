import { Effect, Fiber, Result } from "effect";
import type { APIOptions, Project } from "typescript/unstable/async";
import type { ApplicationSourceLayout } from "../config/source-layout.ts";
import {
	type ClientInspection,
	inspectClientProject,
	type InspectedProjects,
	type InspectionIssues,
	type ServerInspection,
} from "./inspect-projects.ts";
import { withClientProject } from "./typescript-projects.ts";

function testInspectionIssues(failure: {
	readonly _tag: string;
}): failure is InspectionIssues {
	return failure._tag === "InspectionIssues";
}

/** A failure's issues: the issues of inspection issues, and the message of any other failure. */
function listFailureIssues(
	failure: InspectionIssues | { readonly _tag: string; readonly msg: string },
): ReadonlyArray<{ readonly msg: string }> {
	return testInspectionIssues(failure)
		? failure.issues
		: [{ msg: failure.msg }];
}

/**
 * Inspects the application and passes the result to `useFn` while the client
 * checker session is open. `readServer` reads the Rust server's declarations
 * while the client is inspected, and `useFn` receives what it yields. When
 * one side fails, the failure is that side's failure; when both sides fail,
 * the failure is one set of inspection issues that holds the issues of both,
 * with the message of a failure that is not an inspection issue as one issue.
 * `onClientInspectedFn` receives the client's inspection as soon as it exists,
 * while the server's declarations can still be read, so that work that needs
 * only the client's inspection does not wait for the server.
 */
export function withApplicationInspection<
	Server extends { readonly inspection: ServerInspection },
	A,
	E,
	R,
	ServerR,
	ClientE extends { readonly _tag: string; readonly msg: string },
	ClientR,
>(input: {
	readonly layout: ApplicationSourceLayout;
	readonly readServer: Effect.Effect<Server, InspectionIssues, ServerR>;
	readonly fileSystem?: APIOptions["fs"];
	readonly onClientInspectedFn: (inspected: {
		readonly inspection: ClientInspection;
		readonly client: Project;
	}) => Effect.Effect<void, ClientE, ClientR>;
	readonly useFn: (inspected: {
		readonly inspection: InspectedProjects;
		readonly client: Project;
		readonly server: Server;
	}) => Effect.Effect<A, E, R>;
}) {
	const { layout } = input;
	return Effect.scoped(
		Effect.gen(function* () {
			const serverFiber = yield* Effect.forkScoped(input.readServer);
			return yield* withClientProject({
				configDir: layout.configDir,
				clientTsConfigFilePath: layout.clientTsConfigFilePath,
				// Inspection reads the generated file as empty, so it reads every
				// build as the first build does.
				emptiedFiles: [layout.clientTypesFilePath],
				...(input.fileSystem === undefined
					? {}
					: { fileSystem: input.fileSystem }),
				useFn: (client) => {
					return Effect.gen(function* () {
						const [clientInspection, server] = yield* Effect.all(
							[
								inspectClientProject({ client, layout }).pipe(
									Effect.tap((inspection) => {
										return input.onClientInspectedFn({
											inspection,
											client,
										});
									}),
								),
								Fiber.join(serverFiber),
							],
							{ mode: "result" },
						);
						if (
							Result.isFailure(clientInspection) &&
							Result.isFailure(server)
						) {
							return yield* Effect.fail({
								_tag: "InspectionIssues",
								issues: [
									...listFailureIssues(
										clientInspection.failure,
									),
									...listFailureIssues(server.failure),
								],
							} satisfies InspectionIssues);
						}
						if (Result.isFailure(clientInspection)) {
							return yield* Effect.fail(clientInspection.failure);
						}
						if (Result.isFailure(server)) {
							return yield* Effect.fail(server.failure);
						}
						const inspection: InspectedProjects = {
							...server.success.inspection,
							...clientInspection.success,
						};
						return yield* input.useFn({
							inspection,
							client,
							server: server.success,
						});
					});
				},
			});
		}),
	);
}
