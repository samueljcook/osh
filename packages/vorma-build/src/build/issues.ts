import { Console, Effect } from "effect";

/*
Every failure that the build expects is a build issue: a message that is
complete on its own, which the CLI prints and `buildVormaApplication` fails with. An
issue carries no internal structures. An unexpected defect in Vorma or in a
tool that it runs is not an issue, and it keeps its full cause.
*/

/** One problem that the build found, described by its message alone. */
export type VormaBuildIssue = {
	readonly msg: string;
};

/** Every issue that a build found. */
export type VormaBuildIssues = {
	readonly _tag: "VormaBuildIssues";
	readonly issues: ReadonlyArray<VormaBuildIssue>;
};

/** A failure that reports one issue, or one that reports several. */
export type BuildIssueSource =
	| { readonly msg: string }
	| { readonly issues: ReadonlyArray<{ readonly msg: string }> };

export function toBuildIssues(failure: BuildIssueSource): VormaBuildIssues {
	return {
		_tag: "VormaBuildIssues",
		issues:
			"issues" in failure
				? failure.issues.map((issue) => {
						return { msg: issue.msg };
					})
				: [{ msg: failure.msg }],
	};
}

/** Prints each issue's message on standard error. */
export function printBuildIssues(
	issues: VormaBuildIssues,
): Effect.Effect<void> {
	return Effect.forEach(issues.issues, (issue) => Console.error(issue.msg), {
		discard: true,
	});
}

/** A failure of unknown type as build issues, when it is a failure that reports issues. */
export function readBuildIssues(
	failure: unknown,
): VormaBuildIssues | undefined {
	if (typeof failure !== "object" || failure === null) {
		return undefined;
	}
	if ("issues" in failure && Array.isArray(failure.issues)) {
		const issues = failure.issues.flatMap((issue: unknown) => {
			return typeof issue === "object" &&
				issue !== null &&
				"msg" in issue &&
				typeof issue.msg === "string"
				? [{ msg: issue.msg }]
				: [];
		});
		return issues.length === failure.issues.length
			? { _tag: "VormaBuildIssues", issues }
			: undefined;
	}
	return "msg" in failure && typeof failure.msg === "string"
		? { _tag: "VormaBuildIssues", issues: [{ msg: failure.msg }] }
		: undefined;
}

/** An underlying failure's own message, for an issue that explains what it prevented. */
export function describeCause(cause: unknown): string {
	return typeof cause === "object" &&
		cause !== null &&
		"message" in cause &&
		typeof cause.message === "string"
		? cause.message
		: String(cause);
}
