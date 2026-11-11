/*
`vorma`: the configuration API that a Vorma application's configuration file
uses. Only the build reads it. `vorma build` and `vorma dev` are the commands
that build and develop an application.
*/

export { defineConfig, type DevAction, type VormaConfig } from "./config.ts";
