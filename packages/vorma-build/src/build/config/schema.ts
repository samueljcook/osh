import { Schema } from "effect";
import { DEV_ACTIONS, type VormaConfig } from "../../config.ts";

/** An executable followed by its arguments, run without a shell. */
const Cmd = Schema.TupleWithRest(Schema.Tuple([Schema.NonEmptyString]), [
	Schema.String,
]);

/**
 * The command that `dev.server.wrapCmdFn` returns for the compiled executable
 * and the arguments that Vorma would pass to it.
 */
type WrapCmd = NonNullable<
	NonNullable<VormaConfig["dev"]>["server"]["wrapCmdFn"]
>;

const WrapCmdSchema = Schema.declare((value: unknown): value is WrapCmd => {
	return typeof value === "function";
});

const DevOnChangeRule = Schema.Struct({
	triggers: Schema.NonEmptyArray(Schema.NonEmptyString),
	actions: Schema.NonEmptyArray(Schema.Literals(DEV_ACTIONS)),
});

export const VormaConfigSchema = Schema.Struct({
	core: Schema.Struct({
		importMetaUrl: Schema.NonEmptyString,
		distDir: Schema.NonEmptyString,
		staticPublicAssets: Schema.Struct({
			srcDir: Schema.NonEmptyString,
			serveUrlPathPrefix: Schema.String,
		}),
	}),
	build: Schema.Struct({
		serverStartCmd: Cmd,
	}),
	client: Schema.Struct({
		tsConfigFile: Schema.NonEmptyString,
		viteConfigFile: Schema.NonEmptyString,
		tsGenOutFile: Schema.NonEmptyString,
		criticalCssFile: Schema.optionalKey(Schema.NonEmptyString),
		entryFile: Schema.NonEmptyString,
		viewFilePattern: Schema.NonEmptyString,
	}),
	dev: Schema.optionalKey(
		Schema.Struct({
			server: Schema.Struct({
				cargoManifestFile: Schema.NonEmptyString,
				cargoBin: Schema.NonEmptyString,
				cargoArgs: Schema.optionalKey(Schema.Array(Schema.String)),
				binArgs: Schema.optionalKey(Schema.Array(Schema.String)),
				wrapCmdFn: Schema.optionalKey(WrapCmdSchema),
				preferredPort: Schema.optionalKey(
					Schema.Int.check(
						Schema.isBetween({ minimum: 1, maximum: 65_535 }),
					),
				),
			}),
			onChange: Schema.optionalKey(Schema.Array(DevOnChangeRule)),
		}),
	),
}) satisfies Schema.Schema<VormaConfig>;
