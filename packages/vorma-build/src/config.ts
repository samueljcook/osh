/** The work that a `dev.onChange` rule requests. */
export const DEV_ACTIONS = [
	"revalidate-client-data",
	"hard-browser-reload",
	"restart-server",
] as const;

export type DevAction = (typeof DEV_ACTIONS)[number];

/**
 * The input of `vorma build` and `vorma dev`, which take the configuration
 * file as their argument.
 *
 * `core.importMetaUrl` is the configuration file's own `import.meta.url`, and
 * every configured path is relative to that file's directory. `core.distDir`
 * is the directory that contains Vorma's build output: `vorma build` writes the
 * production build to `prod` in it, and `vorma dev` writes the development
 * build to `dev` in it. Vorma owns those two directories and writes nothing
 * else in `core.distDir`.
 * `core.staticPublicAssets.srcDir` contains the static public asset sources, and
 * `serveUrlPathPrefix` is the URL path at which their hashed files are served,
 * with every character that `encodeURI` encodes percent-encoded. The build
 * skips every entry in `srcDir`, at any depth, that is named `.DS_Store` or
 * `.env`, or that has a name starting with `.env.` (in any letter case)
 * because env files contain secrets by convention and everything in the public
 * output is public. A skipped file is not hashed, not published, not a
 * `staticPublicAssets` key, not an input of the build ID, not watched by
 * `vorma dev`, and not served. It publishes every other file, dotfiles
 * included. The key of an asset, in `staticPublicAssets` lookups, in CSS
 * `@public/` URLs, and in the generated types, is its exact path relative to
 * `srcDir`, with `/` between its parts and no leading `/`. Vorma replaces `?`,
 * `#`, `%`, and other non-portable characters in the published file name, and
 * a CSS `@public/` URL names an asset with `%`, `?`, or `#` in its key by
 * percent-encoding those characters as `%25`, `%3F`, and `%23` before any URL
 * query or fragment.
 *
 * The build resolves `staticPublicAssets` through ESM imports, re-exports,
 * namespace accesses, and `const` aliases in the source files of the client
 * TypeScript project, replaces direct string-literal lookups in those files,
 * and fails when code in that project reads the binding in any other way,
 * including through destructuring. For a module outside that project, such as
 * a JavaScript file in a project without `allowJs`, a package's built
 * JavaScript, or a module that a plugin compiles from another file type,
 * `vorma build` and `vorma dev` trace the `@vorma/client` binding through ESM
 * imports, re-exports, namespace accesses, and `const` aliases, allow imports
 * and re-exports that do not read the binding, and fail when the module reads
 * it. A read through a dynamic property key on a namespace, such as
 * `namespace[key]`, throws when it runs because the `staticPublicAssets`
 * object throws for every property access that the build did not replace.
 *
 * `client` names the client project's tsconfig file, the file where the build
 * writes its generated types, the Vite configuration, the browser entry, the
 * view files as one glob pattern, and, optionally, the critical CSS file.
 * `viewFilePattern` is relative to the configuration directory unless it is
 * absolute, also when it starts with `**`, and a pattern that matches no file
 * of the client project fails the build. `tsGenOutFile` must not be inside
 * `core.distDir` or `core.staticPublicAssets.srcDir`. `vorma build` and
 * `vorma dev` check the Vite configuration inside Vite's configuration hook,
 * so Vite evaluates `viteConfigFile` and the plugin factories that
 * `viteConfigFile` calls once per build, once when `vorma dev` starts Vite,
 * and once on each restart of Vite.
 *
 * Plugin assets must have content-hashed names. Emit an asset with `name` and
 * let the bundler name it, or include its complete base32 BLAKE3 content hash
 * in an explicit `fileName`. Plugins must finish chunk changes in `renderChunk`
 * before native hashing; changing hashed chunks in `generateBundle` or
 * `writeBundle` invalidates their immutable URLs.
 *
 * `build.serverStartCmd` is the executable and arguments that `vorma build`
 * runs without a shell from the configuration directory, with the
 * declaration-read variable set so that the application's server writes its
 * declarations and ends. The application builds that server before it runs
 * `vorma build`, or names a command that compiles and runs it. `vorma build`
 * does not compile the application server itself.
 *
 * `dev.server` configures the server that `vorma dev` compiles and runs.
 * `cargoManifestFile` and `cargoBin` name the Cargo target that `vorma dev`
 * compiles after each server change, and `cargoArgs` are added to that Cargo
 * command. `vorma dev` reads the declarations by running the compiled
 * executable with `binArgs`, then runs the same executable with `binArgs` as
 * the development server, passing the application's port in `PORT`. When
 * `wrapCmdFn` is set, the development server runs the command that `wrapCmdFn`
 * returns for the executable and its arguments, for example to run it in a
 * profiler. `wrapCmdFn` does not wrap the declaration reads.
 * Build and development commands run in supervised Unix process groups. When
 * the launcher exits, supervision requests shutdown and allows up to two
 * seconds before forced termination. Command wrappers must keep descendants
 * in that process group. A command's exit terminates its remaining descendants.
 * `preferredPort` is where the search for a free application port starts.
 * `vorma dev` keeps the port that it chose until a configuration change sets
 * another `preferredPort`, and then searches again from the new value. Each
 * `dev.onChange` rule pairs file patterns with the actions that a change to a
 * matching file requests. A pattern is relative to the configuration
 * directory, and `"./"` matches every file; a leading `!` excludes the files
 * that it matches from the patterns before it. The configuration is invalid
 * when a pattern names no file, such as `""`, `"!"`, or `"/"`, or when all of
 * a rule's patterns are exclusions. `"restart-server"` rebuilds
 * and restarts the application's server as a change to a source that Cargo
 * tracks does, for a file that the server reads at run time and that Cargo
 * does not track, such as a configuration file or a template.
 *
 * With a React, Preact, or Solid refresh plugin, route-only edits use Vorma HMR, and ordinary
 * component edits use the renderer's native refresh. An update that affects both view modules
 * and other JS modules reloads the document before applying the mixed update.
 * This resets local component state and keeps providers and consumers on the
 * same module generation, including when invalidation propagates through
 * several modules. A view filename containing a literal percent sign also
 * requires a document reload because Vite's hot-update import does not escape
 * that character.
 *
 * `vorma dev` never writes `prod` in `core.distDir`, and `vorma build` never
 * writes `dev` in it, so a production build stays as it is while `vorma dev`
 * runs.
 */
export type VormaConfig = {
	readonly core: {
		readonly importMetaUrl: string;
		readonly distDir: string;
		readonly staticPublicAssets: {
			readonly srcDir: string;
			readonly serveUrlPathPrefix: string;
		};
	};
	readonly build: {
		readonly serverStartCmd: readonly [string, ...Array<string>];
	};
	readonly client: {
		readonly tsConfigFile: string;
		readonly viteConfigFile: string;
		readonly tsGenOutFile: string;
		readonly criticalCssFile?: string;
		readonly entryFile: string;
		readonly viewFilePattern: string;
	};
	readonly dev?: {
		readonly server: {
			readonly cargoManifestFile: string;
			readonly cargoBin: string;
			readonly cargoArgs?: ReadonlyArray<string>;
			readonly binArgs?: ReadonlyArray<string>;
			readonly wrapCmdFn?: (input: {
				readonly executable: string;
				readonly args: ReadonlyArray<string>;
			}) => readonly [string, ...Array<string>];
			readonly preferredPort?: number;
		};
		readonly onChange?: ReadonlyArray<{
			readonly triggers: readonly [string, ...Array<string>];
			readonly actions: readonly [DevAction, ...Array<DevAction>];
		}>;
	};
};

/** Types a configuration object where it is written. */
export function defineConfig(config: VormaConfig): VormaConfig {
	return config;
}
