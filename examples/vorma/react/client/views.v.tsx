import { createView, Link } from "@vorma/client/react";
import { RenderCounter } from "./counter.tsx";

export default [
	createView({
		pattern: "/",
		// The root remains mounted while its Outlet changes between child pages.
		component: (props) => {
			return (
				<main>
					<h1>{props.serverOutput.title}</h1>
					<nav>
						<Link pattern="/">Home</Link>{" "}
						<Link pattern="/about">About</Link>
					</nav>
					<RenderCounter />
					<props.Outlet />
				</main>
			);
		},
	}),
	createView({
		pattern: "/_index",
		component: (props) => <p>{props.serverOutput}</p>,
	}),
	createView({
		pattern: "/about",
		component: () => (
			<p>The root counter retains its value during navigation.</p>
		),
	}),
];
