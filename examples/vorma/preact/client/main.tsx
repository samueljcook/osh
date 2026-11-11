import { createVormaClient } from "@vorma/client/preact";
import { render } from "preact";

const client = createVormaClient({
	// The application mounts its renderer and returns its synchronous unmount function.
	renderFn: ({ RootOutlet, rootElement }) => {
		render(<RootOutlet />, rootElement);
		return () => render(null, rootElement);
	},
	defaultErrorComponent: (props) => <p role="alert">{props.error.kind}</p>,
});
const result = await client.boot();
if (result.kind === "failure") {
	console.error(result.msg, result.cause);
}
