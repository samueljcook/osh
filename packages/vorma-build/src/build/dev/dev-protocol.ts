/*
The names that `vorma dev` and the application process agree on. The Rust
server reads them.
*/

/** Process-environment names that the server reads. */
export const PROCESS_ENV_PROTOCOL = {
	/**
	 * The dist directory, in a process that `vorma dev` started. The server
	 * reads its build from the build directory of `vorma dev` in it.
	 * `vorma dev` sets the variable for the server that it starts. A server in
	 * any other process ignores it and reads the application's
	 * `dist_dir`.
	 */
	distDirEnvVar: "VORMA_DIST_DIR",
} as const;

/** Private development coordination between the launcher and the application process. */
export const DEV_PROTOCOL = {
	serverPortEnvVar: "PORT",
	/** Set to "1" in a process that `vorma dev` started; the Rust function `vorma::is_dev` reads it. */
	devProcessEnvVar: "__VORMA_IS_DEV",
	/**
	 * Vorma's handler answers a GET of this path in development with 200 and
	 * its build ID; `vorma dev` treats a replacement process as ready only
	 * after that answer.
	 */
	readinessPath: "/.vorma/healthz",
} as const;
