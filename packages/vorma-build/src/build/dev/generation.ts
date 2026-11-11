import { Effect, Fiber } from "effect";
import { collectStaticPublicAssets } from "../assets/static-public-assets.ts";
import {
	assembleApplicationGeneration,
	writeApplicationGeneration,
	type ApplicationGeneration,
} from "../generation/generate-application.ts";
import { toDevClientInputs, type DevClientInputs } from "./client-inputs.ts";
import type { DevLayout } from "./generated-paths.ts";
import { inspectDevSources, type DevSourceInputs } from "./sources.ts";
import type { DevWork } from "./work.ts";

export type DevGeneration = {
	readonly application: ApplicationGeneration;
	readonly sources: DevSourceInputs;
};

/** The inputs that Vite needs from a generation. */
export function toDevGenerationClientInputs(input: {
	readonly layout: DevLayout;
	readonly generation: DevGeneration;
}): DevClientInputs {
	const { application, sources } = input.generation;
	return toDevClientInputs({
		layout: input.layout,
		inspection: application.inspection,
		staticPublicAssets: application.staticPublicAssets,
		resolutionFiles: sources.resolutionFiles,
	});
}

/**
 * Captures independent source and public inputs concurrently, without
 * activating a runtime, and writes the generated client types. The first
 * generation of a session reads every input, and passes Vite's inputs to
 * `onClientInputsFn` as soon as the client's inspection and the static public
 * assets exist, while the server's inspection still runs. A later generation
 * reads only what its work changes: server work inspects the server and the
 * client project and captures the static public assets, client work inspects
 * the client project and reuses the server's inspection, and static work
 * captures the static public assets and reuses both inspections.
 */
export function prepareDevGeneration(input: {
	readonly layout: DevLayout;
	readonly basis:
		| {
				readonly kind: "first";
				readonly onClientInputsFn: (
					inputs: DevClientInputs,
				) => Effect.Effect<void>;
		  }
		| {
				readonly kind: "update";
				readonly generation: DevGeneration;
				readonly work: DevWork;
		  };
}) {
	return Effect.gen(function* () {
		const { basis, layout } = input;
		const capturesStaticPublicAssets =
			basis.kind === "first" ||
			basis.work.serverWork ||
			basis.work.staticWork !== undefined;
		const staticPublicAssets = yield* Effect.forkChild(
			capturesStaticPublicAssets
				? collectStaticPublicAssets(layout)
				: Effect.succeed(
						basis.generation.application.staticPublicAssets,
					),
		);
		const sources =
			basis.kind === "first"
				? inspectDevSources({
						layout,
						server: { _tag: "Inspect" },
						onClientSourcesFn: (client) => {
							return Fiber.join(staticPublicAssets).pipe(
								Effect.flatMap((assets) => {
									return basis.onClientInputsFn(
										toDevClientInputs({
											layout,
											inspection: client.inspection,
											staticPublicAssets: assets,
											resolutionFiles:
												client.resolutionFiles,
										}),
									);
								}),
							);
						},
					})
				: basis.work.serverWork || basis.work.clientWork
					? inspectDevSources({
							layout,
							server: basis.work.serverWork
								? { _tag: "Inspect" }
								: {
										_tag: "Reuse",
										previous: basis.generation.sources,
									},
							onClientSourcesFn: () => Effect.void,
						})
					: Effect.succeed({
							inspection: basis.generation.application.inspection,
							inputs: basis.generation.sources,
						});
		const prepared = yield* Effect.all(
			{
				sources,
				staticPublicAssets: Fiber.join(staticPublicAssets),
			},
			{ concurrency: "unbounded" },
		);
		const application = yield* assembleApplicationGeneration({
			sourceLayout: layout,
			inspection: prepared.sources.inspection,
			staticPublicAssets: prepared.staticPublicAssets,
		});
		yield* writeApplicationGeneration(application);
		return {
			application,
			sources: prepared.sources.inputs,
		} satisfies DevGeneration;
	});
}
