import type {
	CompiledNestedPattern,
	PatternSegment,
} from "primitives-ts/path-matcher";

type NestedRouteNode = {
	readonly compiledPattern: CompiledNestedPattern;
};

/** Builds each unique target's outer-to-inner chain by looking up its compiled path prefixes. */
export function createNestedRouteChains<Node extends NestedRouteNode>(input: {
	readonly nodes: ReadonlyArray<Node>;
}): ReadonlyArray<{
	readonly target: Node;
	readonly chain: ReadonlyArray<Node>;
}> {
	const branches = new Map<string, Node>();
	for (const node of input.nodes) {
		const terminal = node.compiledPattern.segments.at(-1)?.kind;
		if (terminal !== "index" && terminal !== "splat") {
			branches.set(
				getNestedRoutePathKey(node.compiledPattern.segments),
				node,
			);
		}
	}
	return input.nodes.map((target) => {
		const consumingSegments = target.compiledPattern.segments.filter(
			(segment) => segment.kind !== "index" && segment.kind !== "splat",
		);
		const chain: Array<Node> = [];
		for (let depth = 0; depth <= consumingSegments.length; depth++) {
			const branch = branches.get(
				getNestedRoutePathKey(consumingSegments.slice(0, depth)),
			);
			if (branch !== undefined) {
				chain.push(branch);
			}
		}
		const terminal = target.compiledPattern.segments.at(-1)?.kind;
		if (terminal === "index" || terminal === "splat") {
			chain.push(target);
		}
		return { target, chain };
	});
}

function getNestedRoutePathKey(
	segments: ReadonlyArray<PatternSegment>,
): string {
	return JSON.stringify(
		segments.map((segment) => {
			return segment.kind === "static"
				? ["static", segment.value]
				: segment.kind === "dynamic"
					? ["dynamic", segment.paramName]
					: [segment.kind];
		}),
	);
}
