import { useState } from "preact/hooks";

export function RenderCounter() {
	const [count, setCount] = useState(0);
	return (
		<button type="button" onClick={() => setCount((value) => value + 1)}>
			Count: {count}
		</button>
	);
}
