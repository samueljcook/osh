import type { Schema } from "effect";
import type { InspectedProjects } from "../inspection/inspect-projects.ts";

/**
 * The static contracts on which browser code and learned routing depend.
 * Source locations, export names, and runtime implementations are not inputs.
 * This projection is hashed during the build and is never sent to the browser.
 */
export function projectBrowserContracts(
	inspection: InspectedProjects,
): Schema.Json {
	const declarations = inspection.serverDeclarations.map(
		(declaration): readonly [string, Schema.Json] => {
			if (declaration.kind === "ViewLoader") {
				return [
					`ViewLoader ${declaration.pattern}`,
					{
						input: declaration.input ?? "undefined",
						output: declaration.output ?? "undefined",
						errors: declaration.errors.map(
							(error) => error.typeText,
						),
					},
				];
			}
			return [
				`${declaration.method} ${declaration.pattern}`,
				{
					operation: declaration.operation,
					inputTransport: declaration.inputTransport,
					input: declaration.input ?? "undefined",
					output: declaration.output ?? "undefined",
					errors: declaration.errors.map((error) => error.typeText),
				},
			];
		},
	);
	return {
		declarations: Object.fromEntries(declarations),
		typeDeclarations: inspection.typeDeclarations.declarations.map(
			(declaration) => declaration.declaration,
		),
	};
}
