/** Writes a `modulepreload` link for each module path that has none yet. */
export function preloadModules(paths: ReadonlyArray<string>): void {
	if (paths.length === 0) {
		return;
	}
	const existing = new Set(
		[
			...document.head.querySelectorAll<HTMLLinkElement>(
				'link[rel="modulepreload"]',
			),
		].map((link) => link.getAttribute("href")),
	);
	for (const path of paths) {
		if (existing.has(path)) {
			continue;
		}
		const link = document.createElement("link");
		link.rel = "modulepreload";
		link.href = path;
		document.head.appendChild(link);
		existing.add(path);
	}
}
