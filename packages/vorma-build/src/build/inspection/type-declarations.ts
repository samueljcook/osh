/** The named type declarations of a server's contract, and the names that each declaration and each type text references directly. */
export type TypeDeclarations = {
	/** In name order; each declaration is a complete module-local `type` statement at depth zero. */
	readonly declarations: ReadonlyArray<{
		readonly name: string;
		readonly declaration: string;
		readonly referencedTypeNames: ReadonlySet<string>;
	}>;
	readonly typeTexts: ReadonlyMap<string, ReadonlySet<string>>;
};

/**
 * The declarations that the given type texts need, including every
 * declaration that a needed declaration names, in declaration order. A
 * generated file declares only these, so that it has no unused declaration
 * under the application's compiler options.
 */
export function selectTypeDeclarations(input: {
	readonly typeDeclarations: TypeDeclarations;
	readonly typeTexts: Iterable<string>;
}): ReadonlyArray<string> {
	const byName = new Map(
		input.typeDeclarations.declarations.map((declaration) => {
			return [declaration.name, declaration] as const;
		}),
	);
	const needed = new Set<string>();
	const pending: Array<ReadonlySet<string>> = [];
	for (const typeText of input.typeTexts) {
		const referencedTypeNames =
			input.typeDeclarations.typeTexts.get(typeText);
		if (referencedTypeNames !== undefined) {
			pending.push(referencedTypeNames);
		}
	}
	for (
		let referencedTypeNames = pending.pop();
		referencedTypeNames !== undefined;
		referencedTypeNames = pending.pop()
	) {
		for (const name of referencedTypeNames) {
			const declaration = byName.get(name);
			if (!needed.has(name) && declaration !== undefined) {
				needed.add(name);
				pending.push(declaration.referencedTypeNames);
			}
		}
	}
	return input.typeDeclarations.declarations
		.filter((declaration) => needed.has(declaration.name))
		.map((declaration) => declaration.declaration);
}
