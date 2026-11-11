/*
`@vorma/client/react`: the React adapter. It mounts the root outlet and renders
views, error components, and links; it makes the core's state reactive in
React's form; and it passes React's component type through the types. Every
other Vorma function comes from `@vorma/client`.
*/

export { createVormaClient } from "./application.tsx";
export type { VormaClientOptions } from "./application.tsx";
export { useMutation, useQuery, useUrlSync } from "./hooks.ts";
export type { UseMutationOptions, UseUrlSyncOptions } from "./hooks.ts";
export { Link } from "./link.tsx";
export type { DefaultLinkProps, LinkProps } from "./link.tsx";
export { useClientState } from "./client-state.ts";
export { createView } from "./view.tsx";
export type { ViewDef, ViewOptions, ViewProps } from "./view.tsx";
