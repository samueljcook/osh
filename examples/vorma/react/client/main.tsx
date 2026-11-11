import { createVormaClient } from "@vorma/client/react";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

const client = createVormaClient({
	// The application mounts its renderer and returns its synchronous unmount function.
	renderFn: ({ RootOutlet, rootElement }) => {
		const root = createRoot(rootElement);
		root.render(
			<StrictMode>
				<RootOutlet />
			</StrictMode>,
		);
		return () => root.unmount();
	},
	defaultErrorComponent: (props) => <p role="alert">{props.error.kind}</p>,
});
const result = await client.boot();
if (result.kind === "failure") {
	console.error(result.msg, result.cause);
}
