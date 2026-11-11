/*
`@vorma/client/unstable`: the protocol facts and the path wire encoding that
the browser, the build, and generated code share. Anything exported from a
subpath under `unstable` is not public API: it is unsanctioned to build on,
outside semantic versioning, and can change in any release.
*/

export {
	DEV_CONTROLS_PROTOCOL,
	DOCUMENT_PROTOCOL,
	GENERATED_TYPES_PROTOCOL,
	HTTP_PROTOCOL,
	PATTERN_PROTOCOL,
} from "../protocol/constants.ts";
export {
	encodeRoutePattern,
	formatPathWireError,
	type PathWireError,
} from "../protocol/path.ts";
