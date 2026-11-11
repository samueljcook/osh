import { Result } from "effect";
import { transform } from "lightningcss";
import type { Rollup } from "vite";

/**
 * The development critical stylesheet, minified without the application's
 * `build.cssTarget`. `vorma dev` reads the stylesheet from Vite's development
 * CSS pipeline, which does not minify. A production build inlines the
 * stylesheet that Vite's build CSS pipeline produced, which the application's
 * `build.cssMinify` and `build.cssTarget` settings govern.
 */
export function minifyCriticalCss(input: {
	readonly css: string;
}): Result.Result<string, string> {
	return Result.try({
		try: () => {
			return new TextDecoder().decode(
				transform({
					filename: "critical.css",
					code: Buffer.from(input.css),
					minify: true,
				}).code,
			);
		},
		catch: (cause) => `Cannot minify the critical CSS: ${String(cause)}`,
	});
}

/**
 * Finds the configured critical stylesheet through Vite's module-to-CSS
 * association. `sourceId` is the critical CSS entry's module id, the form in
 * which each output names its modules.
 */
export function findCriticalCssFileName(input: {
	readonly sourceId: string | undefined;
	readonly outputs: ReadonlyArray<Rollup.OutputAsset | Rollup.OutputChunk>;
}): Result.Result<string | undefined, string> {
	if (input.sourceId === undefined) {
		return Result.succeed(undefined);
	}
	const sourceId = input.sourceId;
	const stylesheets = new Set(
		input.outputs.flatMap((output) => {
			return output.type === "chunk" &&
				Object.hasOwn(output.modules, sourceId)
				? [...(output.viteMetadata?.importedCss ?? [])]
				: [];
		}),
	);
	const stylesheet = [...stylesheets][0];
	if (stylesheets.size !== 1 || stylesheet === undefined) {
		return Result.fail(
			"The configured critical CSS entry must produce one stylesheet in the client build",
		);
	}
	return Result.succeed(stylesheet);
}
