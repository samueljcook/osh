import { normalizePath } from "vite";
import type { ApplicationGeneration } from "./generate-application.ts";

export type ClientViewEntry = {
	readonly pattern: string;
	/** The view module's id, in the form that Vite's `normalizePath` gives its path; its default export is the view. */
	readonly id: string;
};

/** Projects each validated view declaration into one browser entry. */
export function getClientViewEntries(
	generation: ApplicationGeneration,
): ReadonlyArray<ClientViewEntry> {
	return generation.model.clientViews.map((view) => {
		return { pattern: view.pattern, id: normalizePath(view.filePath) };
	});
}
