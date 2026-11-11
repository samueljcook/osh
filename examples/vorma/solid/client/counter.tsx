import { createSignal } from "solid-js";

export function RenderCounter() {
	const [count, setCount] = createSignal(0);
	return (
		<button type="button" onClick={() => setCount((value) => value + 1)}>
			Count: {count()}
		</button>
	);
}
